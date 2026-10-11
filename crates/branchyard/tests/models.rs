//! The model gateway through the engine: a fake harness (a Python script
//! the fake ACP agent runs) calls mock Anthropic and OpenAI upstreams on
//! loopback through its turn's gateway, by the base URL and key variables
//! a real harness reads. The gateway injects the backend's key, which the
//! harness never sees; refuses models outside the token's scope, calls
//! over a rate limit or a budget, and tokens not its turn's; fails over on
//! a refused connection, a 5xx and a 429; records a usage row and a
//! `model` event per call, and the branch's cost is exactly what its calls
//! cost. Under a required egress policy the confined harness reaches the
//! gateway and nothing else. Hermetic: nothing leaves this host.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use branchyard::models::{self, KeySource, ModelActivity, ModelCall};
use branchyard::{
    Activity, Budget, Ceiling, Network, NetworkEnforce, Policy, Provisioning, RecordedEvent,
    TaskOptions,
};
use common::{text, Fixture};
use serde_json::{json, Value};

const ANTHROPIC_KEY: &str = "sk-real-anthropic-1";
const OPENAI_KEY: &str = "sk-real-openai-1";
/// Words the harness puts in the prompt of its `tools` calls, which the
/// event log must never hold.
const MARKER: &str = "secret-marker-31";

/// What a mock upstream answers.
#[derive(Clone, Copy)]
enum Behavior {
    /// As the provider would, with usage.
    Answer,
    /// This status and an error body.
    Status(u16),
}

/// A request a mock upstream received.
#[derive(Debug)]
struct Seen {
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
    /// The body's bytes, as the upstream received them.
    raw: Vec<u8>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

struct Mock {
    port: u16,
    requests: Receiver<Seen>,
}

impl Mock {
    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn seen(&self) -> Vec<Seen> {
        self.requests.try_iter().collect()
    }
}

fn mock(behavior: Behavior) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sender, requests) = mpsc::channel();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let sender = sender.clone();
            thread::spawn(move || serve(stream, behavior, sender));
        }
    });
    Mock { port, requests }
}

fn serve(stream: std::net::TcpStream, behavior: Behavior, sender: mpsc::Sender<Seen>) {
    let mut out = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    if reader.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let path = first.split_whitespace().nth(1).unwrap_or("").to_owned();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.trim_end().split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    let length: usize = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut raw = vec![0u8; length];
    reader.read_exact(&mut raw).unwrap();
    let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    let model = body["model"].as_str().unwrap_or("").to_owned();
    let stream = body["stream"] == json!(true);
    let usage_asked = body["stream_options"]["include_usage"] == json!(true);
    // A request that offers tools gets an answer that calls some, as a
    // model would.
    let tools = body["tools"].as_array().is_some_and(|t| !t.is_empty());
    branchyard_support::best_effort(
        "report the request to the test",
        sender.send(Seen {
            path: path.clone(),
            headers,
            body,
            raw,
        }),
    );
    if let Behavior::Status(code) = behavior {
        let body = format!(r#"{{"error":{{"type":"upstream_{code}","message":"mock"}}}}"#);
        branchyard_support::best_effort(
            "answer the mock request",
            write!(
                out,
                "HTTP/1.1 {code} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
                body.len()
            ),
        );
        return;
    }
    let events: Vec<String> = match (path.as_str(), stream) {
        // Offered tools: it calls Read, then Edit.
        ("/v1/messages", true) if tools => vec![
            format!(
                "event: message_start\ndata: {}\n\n",
                json!({"type": "message_start", "message": {"model": model, "stop_reason": null,
                    "usage": {"input_tokens": 10, "output_tokens": 1, "cache_read_input_tokens": 100,
                    "cache_creation_input_tokens": 40,
                    "cache_creation": {"ephemeral_5m_input_tokens": 30, "ephemeral_1h_input_tokens": 10}}}})
            ),
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n".to_owned(),
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Read\",\"input\":{}}}\n\n".to_owned(),
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n".to_owned(),
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"Edit\",\"input\":{}}}\n\n".to_owned(),
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n".to_owned(),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_owned(),
        ],
        ("/v1/chat/completions", true) if tools => {
            let delta = |calls: Value, finish: Value| {
                format!(
                    "data: {}\n\n",
                    json!({"model": model, "choices": [{"index": 0, "delta": calls,
                        "finish_reason": finish}]})
                )
            };
            let mut events = vec![
                delta(
                    json!({"role": "assistant", "tool_calls": [{"index": 0, "id": "call_1",
                        "type": "function", "function": {"name": "Read", "arguments": ""}}]}),
                    Value::Null,
                ),
                delta(
                    json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"path\":\"a\"}"}}]}),
                    Value::Null,
                ),
                delta(
                    json!({"tool_calls": [{"index": 1, "id": "call_2", "type": "function",
                        "function": {"name": "Edit", "arguments": "{}"}}]}),
                    Value::Null,
                ),
                delta(json!({}), json!("tool_calls")),
            ];
            if usage_asked {
                events.push(format!(
                    "data: {}\n\n",
                    json!({"model": model, "choices": [], "usage": {"prompt_tokens": 100,
                        "completion_tokens": 20, "prompt_tokens_details": {"cached_tokens": 60}}})
                ));
            }
            events.push("data: [DONE]\n\n".to_owned());
            events
        }
        ("/v1/responses", true) if tools => {
            let call = |name: &str| {
                json!({"type": "function_call", "call_id": format!("c-{name}"), "name": name,
                    "arguments": "{}"})
            };
            vec![
                format!(
                    "event: response.created\ndata: {}\n\n",
                    json!({"type": "response.created", "response": {"model": model,
                        "status": "in_progress"}})
                ),
                format!(
                    "event: response.output_item.added\ndata: {}\n\n",
                    json!({"type": "response.output_item.added", "item": call("Read")})
                ),
                format!(
                    "event: response.output_item.done\ndata: {}\n\n",
                    json!({"type": "response.output_item.done", "item": call("Read")})
                ),
                format!(
                    "event: response.output_item.done\ndata: {}\n\n",
                    json!({"type": "response.output_item.done", "item": call("Edit")})
                ),
                format!(
                    "event: response.completed\ndata: {}\n\n",
                    json!({"type": "response.completed", "response": {"model": model,
                        "status": "completed", "output": [call("Read"), call("Edit")],
                        "usage": {"input_tokens": 30, "output_tokens": 4,
                            "input_tokens_details": {"cached_tokens": 10}}}})
                ),
            ]
        }
        ("/v1/messages", true) => vec![
            format!(
                "event: message_start\ndata: {}\n\n",
                json!({"type": "message_start", "message": {"model": model, "usage": {
                    "input_tokens": 10, "output_tokens": 1, "cache_read_input_tokens": 100,
                    "cache_creation_input_tokens": 40,
                    "cache_creation": {"ephemeral_5m_input_tokens": 30, "ephemeral_1h_input_tokens": 10}}}})
            ),
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n".to_owned(),
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n".to_owned(),
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_owned(),
        ],
        ("/v1/chat/completions", true) => {
            let mut events = vec![format!(
                "data: {}\n\n",
                json!({"model": model, "choices": [{"delta": {"content": "hello"}}]})
            )];
            if usage_asked {
                events.push(format!(
                    "data: {}\n\n",
                    json!({"model": model, "choices": [], "usage": {"prompt_tokens": 100,
                        "completion_tokens": 20, "prompt_tokens_details": {"cached_tokens": 60}}})
                ));
            }
            events.push("data: [DONE]\n\n".to_owned());
            events
        }
        ("/v1/responses", true) => vec![
            format!(
                "event: response.created\ndata: {}\n\n",
                json!({"type": "response.created", "response": {"model": model}})
            ),
            format!(
                "event: response.completed\ndata: {}\n\n",
                json!({"type": "response.completed", "response": {"model": model, "usage": {
                    "input_tokens": 30, "output_tokens": 4,
                    "input_tokens_details": {"cached_tokens": 10}}}})
            ),
        ],
        _ => Vec::new(),
    };
    if !events.is_empty() {
        // Streamed in chunks, each written on its own.
        branchyard_support::best_effort(
            "start the mock event stream",
            write!(
            out,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\
             Connection: close\r\n\r\n"
        ),
        );
        for event in events {
            branchyard_support::best_effort(
                "write a mock event",
                write!(out, "{:x}\r\n{event}\r\n", event.len()),
            );
            branchyard_support::best_effort("flush a mock event", out.flush());
        }
        branchyard_support::best_effort("end the mock event stream", write!(out, "0\r\n\r\n"));
        return;
    }
    let body = match (path.as_str(), tools) {
        // Offered tools: it calls Read, then Edit.
        ("/v1/messages", true) => json!({"type": "message", "model": model,
            "content": [{"type": "text", "text": "hello"},
                {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "a"}},
                {"type": "tool_use", "id": "toolu_2", "name": "Edit", "input": {}}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 100,
                "cache_creation_input_tokens": 40,
                "cache_creation": {"ephemeral_5m_input_tokens": 30, "ephemeral_1h_input_tokens": 10}}}),
        ("/v1/responses", true) => json!({"object": "response", "model": model,
            "status": "completed",
            "output": [
                {"type": "function_call", "call_id": "c-Read", "name": "Read", "arguments": "{}"},
                {"type": "function_call", "call_id": "c-Edit", "name": "Edit", "arguments": "{}"}],
            "usage": {"input_tokens": 30, "output_tokens": 4,
                "input_tokens_details": {"cached_tokens": 10}}}),
        (_, true) => json!({"model": model, "choices": [{"index": 0,
            "message": {"content": null, "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "Read", "arguments": "{}"}},
                {"id": "call_2", "type": "function", "function": {"name": "Edit", "arguments": "{}"}}]},
            "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20,
                "prompt_tokens_details": {"cached_tokens": 60}}}),
        ("/v1/messages", false) => json!({"type": "message", "model": model,
            "content": [{"type": "text", "text": "hello"}],
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 100,
                "cache_creation_input_tokens": 40,
                "cache_creation": {"ephemeral_5m_input_tokens": 30, "ephemeral_1h_input_tokens": 10}}}),
        (_, false) => json!({"model": model, "choices": [{"message": {"content": "hello"}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20,
                "prompt_tokens_details": {"cached_tokens": 60}}}),
    }
    .to_string();
    branchyard_support::best_effort(
        "answer the mock request",
        write!(
            out,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
            body.len()
        ),
    );
}

/// The fake harness: `harness.py direct|proxy CALL...`. A call is
/// `API,PATH,MODEL,json|stream|tools|toolstream[,KEY]` (PATH after the base
/// URL), sent the way that provider's SDK sends it, printing `call MODEL
/// MODE: STATUS events=N [ERROR TYPE] [retry=SECONDS]`; `tools` and
/// `toolstream` are `json` and `stream` offering the tools Read, Edit and
/// Bash, with a marker in the prompt ([`MARKER`]); `token=FILE` writes the
/// harness's ANTHROPIC_API_KEY to FILE; `sent=DIR` writes each later
/// call's request body, as sent, to `DIR/1`, `DIR/2`, and so on, and the
/// answer it read to `DIR/1.response`, `DIR/2.response`. It first prints
/// the names of any variables holding a real key (`sk-real...`). `direct`
/// ignores the proxy variables; `proxy` honors them.
fn harness(dir: &Path) -> PathBuf {
    let path = dir.join("harness.py");
    std::fs::write(
        &path,
        r#"import json, os, sys, urllib.error, urllib.request
MARKER = "secret-marker-31"
mode = sys.argv[1]
handlers = [] if mode == "proxy" else [urllib.request.ProxyHandler({})]
opener = urllib.request.build_opener(*handlers)
real = sorted(k for k, v in os.environ.items() if "sk-real" in v)
print("real keys in env:", ",".join(real) or "none")
sent, count = None, 0
for spec in sys.argv[2:]:
    if spec.startswith("token="):
        with open(spec[6:], "w") as f:
            f.write(os.environ.get("ANTHROPIC_API_KEY", ""))
        continue
    if spec.startswith("sent="):
        sent = spec[5:]
        continue
    parts = spec.split(",")
    api, path, model, kind = parts[:4]
    tools = kind in ("tools", "toolstream")
    ask = "hi " + MARKER if tools else "hi"
    if api == "anthropic":
        base = os.environ["ANTHROPIC_BASE_URL"]
        key = parts[4] if len(parts) > 4 else os.environ["ANTHROPIC_API_KEY"]
        headers = {"x-api-key": key, "anthropic-version": "2023-06-01"}
        body = {"model": model, "max_tokens": 16, "messages": [{"role": "user", "content": ask}]}
    else:
        base = os.environ["OPENAI_BASE_URL"]
        key = parts[4] if len(parts) > 4 else os.environ["OPENAI_API_KEY"]
        headers = {"Authorization": "Bearer " + key}
        if path.endswith("/responses"):
            body = {"model": model, "input": ask}
        else:
            body = {"model": model, "messages": [{"role": "user", "content": ask}]}
    if tools:
        names = ["Read", "Edit", "Bash"]
        if api == "anthropic":
            body["tools"] = [{"name": n, "description": n, "input_schema": {"type": "object"}} for n in names]
        elif path.endswith("/responses"):
            body["tools"] = [{"type": "function", "name": n, "parameters": {"type": "object"}} for n in names]
        else:
            body["tools"] = [{"type": "function", "function": {"name": n, "parameters": {"type": "object"}}} for n in names]
    if kind in ("stream", "toolstream"):
        body["stream"] = True
    headers["content-type"] = "application/json"
    payload = json.dumps(body).encode() if kind != "broken" else b'{"model": "' + model.encode()
    if sent:
        count += 1
        with open(os.path.join(sent, str(count)), "wb") as f:
            f.write(payload)
    request = urllib.request.Request(base + path, data=payload, headers=headers, method="POST")
    retry = None
    try:
        with opener.open(request, timeout=60) as response:
            status, data = response.status, response.read().decode()
    except urllib.error.HTTPError as error:
        status, data, retry = error.code, error.read().decode(), error.headers.get("Retry-After")
    except OSError as error:
        print(f"call {model} {kind}: failed {error}")
        continue
    if sent:
        with open(os.path.join(sent, str(count) + ".response"), "w") as f:
            f.write(data)
    events = sum(1 for line in data.splitlines() if line.startswith("data:"))
    line = f"call {model} {kind}: {status} events={events}"
    if status >= 400:
        try:
            line += " " + json.loads(data)["error"]["type"]
        except Exception:
            line += " ?"
    if retry:
        line += f" retry={retry}"
    if status == 200 and kind == "json" and api == "anthropic":
        try:
            line += " text=" + json.loads(data)["content"][0]["text"]
        except Exception:
            line += " text=?"
    print(line)
"#,
    )
    .unwrap();
    path
}

fn prompt(script: &Path, mode: &str, calls: &[&str]) -> String {
    format!("SH python3 {} {mode} {}", script.display(), calls.join(" "))
}

/// Give the fixture's yard a model gateway, its keys in files beside the
/// repository.
fn use_gateway(f: &Fixture, config: Value) {
    std::fs::write(f.dir.join("anthropic"), format!("{ANTHROPIC_KEY}\n")).unwrap();
    std::fs::write(f.dir.join("openai"), OPENAI_KEY).unwrap();
    let config: models::Config = serde_json::from_value(config).unwrap();
    let signer = models::Signer::local(&f.yard).unwrap();
    let dir = f.dir.clone();
    let gateway =
        models::Gateway::new(&config, signer, move |name| KeySource::File(dir.join(name))).unwrap();
    f.yard.use_models(gateway);
}

fn on_gateway(models: &[&str]) -> Provisioning {
    Provisioning {
        models: Some(models::ModelAccess {
            allow: models.iter().map(|m| (*m).to_owned()).collect(),
        }),
        ..Provisioning::default()
    }
}

fn options(f: &Fixture, provision: Provisioning) -> TaskOptions {
    TaskOptions {
        policy: Policy::allow_all(),
        provision: Some(provision),
        budget: Budget {
            max_duration: Some(Duration::from_secs(120)),
            ..Budget::default()
        },
        ..f.options()
    }
}

fn calls(events: &[RecordedEvent]) -> Vec<ModelCall> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Model(m) => match m.as_ref() {
                ModelActivity::Call(call) => Some(call.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn said(events: &[RecordedEvent], lines: &[&str]) {
    let said = text(events);
    for line in lines {
        assert!(said.contains(line), "{line:?} not in {said}");
    }
}

#[test]
fn a_harness_calls_both_providers_through_its_gateway_and_never_holds_the_key() {
    let f = Fixture::new();
    let (anthropic, openai) = (mock(Behavior::Answer), mock(Behavior::Answer));
    use_gateway(
        &f,
        json!({
            "backends": {
                "anthropic": {"api": "anthropic", "url": anthropic.url(), "key": "anthropic"},
                "openai": {"api": "openai", "url": format!("{}/", openai.url()), "key": "openai"}
            },
            "routes": [{"model": "claude-*", "backends": ["anthropic"]}]
        }),
    );
    let script = harness(&f.dir);
    let token_file = f.dir.join("token");
    let token_spec = format!("token={}", token_file.display());
    let branch = f
        .task(&prompt(
            &script,
            "direct",
            &[
                "anthropic,/v1/messages,claude-sonnet-4-6,json",
                "anthropic,/v1/messages,claude-sonnet-4-6,stream",
                "openai,/chat/completions,gpt-5,json",
                "openai,/chat/completions,gpt-5,stream",
                "openai,/responses,gpt-5,stream",
                &token_spec,
            ],
        ))
        .options(options(&f, on_gateway(&["claude-sonnet-*", "gpt-5"])))
        .name("both")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    said(
        &events,
        &[
            "real keys in env: none",
            "call claude-sonnet-4-6 json: 200 events=0",
            "call claude-sonnet-4-6 stream: 200 events=4",
            "call gpt-5 json: 200 events=0",
            "call gpt-5 stream: 200 events=3",
            "call gpt-5 stream: 200 events=2",
        ],
    );
    // Each upstream saw the real key, never the token, and plain bytes.
    let seen = anthropic.seen();
    assert_eq!(seen.len(), 2);
    for request in &seen {
        assert_eq!(request.path, "/v1/messages");
        assert_eq!(request.header("x-api-key"), Some(ANTHROPIC_KEY));
        assert_eq!(request.header("authorization"), None);
        assert_eq!(request.header("accept-encoding"), Some("identity"));
        assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
    }
    let seen = openai.seen();
    assert_eq!(
        seen.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        [
            "/v1/chat/completions",
            "/v1/chat/completions",
            "/v1/responses"
        ]
    );
    for request in &seen {
        let bearer = format!("Bearer {OPENAI_KEY}");
        assert_eq!(request.header("authorization"), Some(bearer.as_str()));
    }
    // A chat stream was asked for its usage; nothing else was changed.
    assert_eq!(seen[1].body["stream_options"]["include_usage"], true);
    assert_eq!(seen[0].body.get("stream_options"), None);
    assert_eq!(seen[2].body.get("stream_options"), None);
    // One allowed call each, with its tokens.
    let recorded = calls(&events);
    assert_eq!(recorded.len(), 5, "{recorded:?}");
    assert!(recorded
        .iter()
        .all(|c| c.decision == "allowed" && c.status == 200));
    assert_eq!(
        recorded.iter().map(|c| c.streamed).collect::<Vec<_>>(),
        [false, true, false, true, true]
    );
    // Each carries an interception record: one message in the request, no
    // tools offered or called, and sizes. Digests only: no contents kept.
    for call in &recorded {
        let seen = call.interception.as_ref().expect("an interception");
        assert_eq!(
            (
                seen.messages,
                seen.tools_offered.len(),
                seen.tool_uses.len()
            ),
            (1, 0, 0),
            "{call:?}"
        );
        assert!(
            seen.request_bytes > 0 && seen.response_bytes > 0,
            "{call:?}"
        );
        assert_eq!(seen.prompt_digest.len(), 64, "{call:?}");
        assert_eq!(call.audit_file, None);
    }
    assert!(!f.root.join(".branchyard/models/audit").exists());
    let sonnet = models::Tokens {
        input: 10,
        output: 5,
        cache_read: 100,
        cache_write: 30,
        cache_write_1h: 10,
    };
    assert_eq!(recorded[0].tokens, Some(sonnet));
    assert_eq!(recorded[1].tokens, Some(sonnet));
    let chat = models::Tokens {
        input: 40,
        output: 20,
        cache_read: 60,
        ..models::Tokens::default()
    };
    assert_eq!(recorded[2].tokens, Some(chat));
    assert_eq!(recorded[3].tokens, Some(chat));
    assert_eq!(
        recorded[4].tokens,
        Some(models::Tokens {
            input: 20,
            output: 4,
            cache_read: 10,
            ..models::Tokens::default()
        })
    );
    // Priced from the catalog: Sonnet 4.6 $3 / $15 / $0.30 / $3.75 / $6
    // and gpt-5 $1.25 / $0.125 cached / $10 per million.
    let sonnet_cost = (10.0 * 3.0 + 5.0 * 15.0 + 100.0 * 0.3 + 30.0 * 3.75 + 10.0 * 6.0) / 1e6;
    let chat_cost = (40.0 * 1.25 + 60.0 * 0.125 + 20.0 * 10.0) / 1e6;
    let responses_cost = (20.0 * 1.25 + 10.0 * 0.125 + 4.0 * 10.0) / 1e6;
    let expected = [
        sonnet_cost,
        sonnet_cost,
        chat_cost,
        chat_cost,
        responses_cost,
    ];
    for (call, want) in recorded.iter().zip(expected) {
        assert!((call.cost_usd.unwrap() - want).abs() < 1e-12, "{call:?}");
    }
    // The store has one row per call, and the branch's cost is exactly
    // what they cost.
    let rows = f.yard.model_usage(0).unwrap();
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|r| r.branch == "both" && r.turn == 1));
    assert_eq!(
        rows.iter().map(|r| r.backend.as_str()).collect::<Vec<_>>(),
        ["anthropic", "anthropic", "openai", "openai", "openai"]
    );
    let total: f64 = rows.iter().map(|r| r.cost_usd.unwrap()).sum();
    assert_eq!(branch.info().cost_usd, Some(total));
    assert!((total - expected.iter().sum::<f64>()).abs() < 1e-12);
    // The key is nowhere in what was recorded.
    let log = serde_json::to_string(&events).unwrap();
    assert!(!log.contains("sk-real"), "a key was recorded");
    // The harness held the turn's token, which carries the turn's scopes.
    let token = std::fs::read_to_string(&token_file).unwrap();
    let jwks: Value = serde_json::from_str(
        &std::fs::read_to_string(f.root.join(".branchyard/gateway/jwks.json")).unwrap(),
    )
    .unwrap();
    let claims = branchyard::connectors::keys::verify(&jwks, &token).unwrap();
    assert_eq!(claims["by_models"], json!(["claude-sonnet-*", "gpt-5"]));
    assert_eq!(claims["by_branch"], "both");
    assert_eq!(claims["by_turn"], "1");
    assert_eq!(claims["by_grants"], json!([]));
    assert_eq!(claims["by_network"]["policy"], "open");
    assert_eq!(claims["by_delegation"]["max_depth"], 0);
    assert!(claims["iss"]
        .as_str()
        .unwrap()
        .starts_with("branchyard:local:"));
    assert!(!log.contains(&token), "the token was recorded");
    // The turn says where its models went.
    let gateway = events.iter().find_map(|e| match &e.activity {
        Activity::Model(m) => match m.as_ref() {
            ModelActivity::Gateway { url, models, .. } => Some((url.clone(), models.clone())),
            _ => None,
        },
        _ => None,
    });
    let (url, allowed) = gateway.expect("a gateway activity");
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    assert_eq!(allowed, ["claude-sonnet-*", "gpt-5"]);
    // The gateway announced itself in the registry for its turn, by what it
    // serves, and left when the turn ended (docs/registry.md).
    use branchyard::services::ServiceStore;
    let announced: Vec<_> = f
        .yard
        .services()
        .unwrap()
        .all()
        .unwrap()
        .into_iter()
        .filter(|s| s.kind == branchyard::services::KIND_MODEL_GATEWAY)
        .collect();
    assert_eq!(announced.len(), 1, "{announced:?}");
    let service = &announced[0];
    assert_eq!(service.owner.branch.as_deref(), Some("both"));
    assert_eq!(service.url(), Some(url.as_str()));
    assert!(service.state.ended(), "{service:?}");
    assert!(service.reclaim.is_none(), "nothing to reclaim: {service:?}");
}

#[test]
fn the_gateway_refuses_what_the_token_does_not_allow() {
    let f = Fixture::new();
    let anthropic = mock(Behavior::Answer);
    use_gateway(
        &f,
        json!({"backends": {"anthropic": {"api": "anthropic", "url": anthropic.url(),
            "key": "anthropic"}}}),
    );
    let script = harness(&f.dir);
    let branch = f
        .task(&prompt(
            &script,
            "direct",
            &[
                "anthropic,/v1/messages,claude-sonnet-4-6,json",
                "anthropic,/v1/messages,claude-haiku-4-5,json,not-the-token",
                "anthropic,/v1/messages,claude-haiku-4-5,broken",
                "anthropic,/v1/messages,claude-haiku-4-5,json",
            ],
        ))
        .options(options(&f, on_gateway(&["claude-haiku-*"])))
        .name("scoped")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    said(
        &events,
        &[
            "call claude-sonnet-4-6 json: 403 events=0 permission_error",
            "call claude-haiku-4-5 json: 401 events=0 authentication_error",
            "call claude-haiku-4-5 broken: 400 events=0 api_error",
            "call claude-haiku-4-5 json: 200 events=0",
        ],
    );
    // Only the allowed call reached the upstream.
    let seen = anthropic.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].body["model"], "claude-haiku-4-5");
    let recorded = calls(&events);
    assert_eq!(
        recorded
            .iter()
            .map(|c| c.decision.as_str())
            .collect::<Vec<_>>(),
        ["denied", "unauthorized", "allowed"]
    );
    assert!(recorded[0]
        .reason
        .as_deref()
        .unwrap()
        .contains("claude-haiku-*"));
    assert_eq!(f.yard.model_usage(0).unwrap().len(), 1);
    // A call the gateway refuses was not forwarded and has no
    // interception; the one it forwarded has.
    assert_eq!(
        recorded
            .iter()
            .map(|c| c.interception.is_some())
            .collect::<Vec<_>>(),
        [false, false, true]
    );
}

#[test]
fn backends_fail_over_on_refusal_5xx_and_429_in_order() {
    let f = Fixture::new();
    let refused = TcpListener::bind("127.0.0.1:0").unwrap();
    let refused_url = format!("http://127.0.0.1:{}", refused.local_addr().unwrap().port());
    drop(refused);
    let (five, many, good) = (
        mock(Behavior::Status(503)),
        mock(Behavior::Status(429)),
        mock(Behavior::Answer),
    );
    use_gateway(
        &f,
        json!({
            "backends": {
                "refused": {"api": "anthropic", "url": refused_url, "key": "anthropic"},
                "five": {"api": "anthropic", "url": five.url(), "key": "anthropic"},
                "many": {"api": "anthropic", "url": many.url(), "key": "anthropic"},
                "good": {"api": "anthropic", "url": good.url(), "key": "anthropic"},
                "last": {"api": "anthropic", "url": five.url(), "key": "anthropic"}
            },
            "routes": [
                {"model": "claude-sonnet-*", "backends": ["refused"],
                 "fallbacks": ["five", "many", "good"]},
                {"model": "claude-opus-*", "backends": ["five"], "fallbacks": ["last"]}
            ]
        }),
    );
    let script = harness(&f.dir);
    let branch = f
        .task(&prompt(
            &script,
            "direct",
            &[
                "anthropic,/v1/messages,claude-sonnet-4-6,stream",
                "anthropic,/v1/messages,claude-opus-4-6,json",
            ],
        ))
        .options(options(&f, on_gateway(&["*"])))
        .name("failover")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    said(
        &events,
        &[
            "call claude-sonnet-4-6 stream: 200 events=4",
            // Every backend failed: the harness gets the last one's answer.
            "call claude-opus-4-6 json: 503 events=0 upstream_503",
        ],
    );
    let recorded = calls(&events);
    assert_eq!(recorded[0].decision, "allowed");
    assert_eq!(recorded[0].backend.as_deref(), Some("good"));
    assert_eq!(
        recorded[0].failed_over.len(),
        3,
        "{:?}",
        recorded[0].failed_over
    );
    assert!(recorded[0].failed_over[0].starts_with("refused: could not connect"));
    assert_eq!(recorded[0].failed_over[1], "five: 503");
    assert_eq!(recorded[0].failed_over[2], "many: 429");
    assert_eq!(recorded[1].decision, "failed");
    assert_eq!(recorded[1].status, 503);
    assert_eq!(recorded[1].backend.as_deref(), Some("last"));
    // The last backend's answer is an interception too: its size, no tools.
    let answer = r#"{"error":{"type":"upstream_503","message":"mock"}}"#;
    let seen = recorded[1].interception.as_ref().unwrap();
    assert_eq!(seen.response_bytes, answer.len() as u64);
    assert_eq!(
        (seen.tool_uses.len(), seen.stop_reason.as_deref()),
        (0, None)
    );
    assert!(recorded[0].interception.is_some());
    assert_eq!(five.seen().len(), 3);
    assert_eq!(many.seen().len(), 1);
    assert_eq!(good.seen().len(), 1);
    // Only the answered call is usage.
    let rows = f.yard.model_usage(0).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].backend, "good");
}

#[test]
fn rate_limits_and_budgets_refuse_before_anything_is_forwarded() {
    let f = Fixture::new();
    let anthropic = mock(Behavior::Answer);
    // Haiku 4.5's call costs $0.0001025 and Sonnet 4.6's $0.0003075 (see
    // the first test); the day allows $0.0004, so a third call is refused.
    use_gateway(
        &f,
        json!({
            "backends": {"anthropic": {"api": "anthropic", "url": anthropic.url(),
                "key": "anthropic"}},
            "routes": [
                {"model": "claude-haiku-*", "backends": ["anthropic"], "requests_per_minute": 1},
                {"model": "claude-sonnet-*", "backends": ["anthropic"]}
            ],
            "budget": {"daily_usd": 0.0004, "alert_at": 0.5}
        }),
    );
    let script = harness(&f.dir);
    let branch = f
        .task(&prompt(
            &script,
            "direct",
            &[
                "anthropic,/v1/messages,claude-haiku-4-5,json",
                "anthropic,/v1/messages,claude-haiku-4-5,json",
                "anthropic,/v1/messages,claude-sonnet-4-6,json",
                "anthropic,/v1/messages,claude-sonnet-4-6,json",
            ],
        ))
        .options(options(&f, on_gateway(&["*"])))
        .name("limits")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    said(
        &events,
        &[
            "call claude-haiku-4-5 json: 200",
            "call claude-haiku-4-5 json: 429 events=0 rate_limit_error retry=",
            "call claude-sonnet-4-6 json: 200",
            "call claude-sonnet-4-6 json: 403 events=0 permission_error",
        ],
    );
    let recorded = calls(&events);
    assert_eq!(
        recorded
            .iter()
            .map(|c| c.decision.as_str())
            .collect::<Vec<_>>(),
        ["allowed", "rate_limited", "allowed", "budget"]
    );
    assert!(recorded[3].reason.as_deref().unwrap().contains("daily_usd"));
    assert_eq!(anthropic.seen().len(), 2);
    // The day's spending passed half its budget: one alert.
    let alerts: Vec<&ModelActivity> = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Model(m) if matches!(m.as_ref(), ModelActivity::Alert { .. }) => {
                Some(m.as_ref())
            }
            _ => None,
        })
        .collect();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert!(
        alerts[0].describe().contains("this day"),
        "{}",
        alerts[0].describe()
    );
    // The branch's own budget: spent by the first call, so the second is
    // refused before it is forwarded.
    let g = Fixture::new();
    let upstream = mock(Behavior::Answer);
    use_gateway(
        &g,
        json!({"backends": {"anthropic": {"api": "anthropic", "url": upstream.url(),
            "key": "anthropic"}}}),
    );
    let script = harness(&g.dir);
    let mut opts = options(&g, on_gateway(&["*"]));
    opts.budget.max_usd = Some(0.0002);
    let branch = g
        .task(&prompt(
            &script,
            "direct",
            &[
                "anthropic,/v1/messages,claude-sonnet-4-6,json",
                "anthropic,/v1/messages,claude-sonnet-4-6,json",
            ],
        ))
        .options(opts)
        .name("spent")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    let recorded = calls(&events);
    assert_eq!(recorded[0].decision, "allowed");
    assert_eq!(recorded[1].decision, "budget");
    assert!(recorded[1].reason.as_deref().unwrap().contains("max_usd"));
    assert_eq!(upstream.seen().len(), 1);
    let rows = g.yard.model_usage(0).unwrap();
    assert_eq!(branch.info().cost_usd, rows[0].cost_usd);
}

#[test]
fn a_persons_ceiling_narrows_what_the_token_allows() {
    let f = Fixture::new();
    let anthropic = mock(Behavior::Answer);
    use_gateway(
        &f,
        json!({"backends": {"anthropic": {"api": "anthropic", "url": anthropic.url(),
            "key": "anthropic"}}}),
    );
    let subject = branchyard::connectors::local_subject();
    f.yard.use_ceilings(
        [(
            subject.clone(),
            Ceiling {
                models: Some(models::ModelAccess {
                    allow: vec!["claude-haiku-*".into()],
                }),
                ..Ceiling::default()
            },
        )]
        .into_iter()
        .collect(),
    );
    let script = harness(&f.dir);
    let token_file = f.dir.join("token");
    let token_spec = format!("token={}", token_file.display());
    let branch = f
        .task(&prompt(
            &script,
            "direct",
            &[
                "anthropic,/v1/messages,claude-sonnet-4-6,json",
                "anthropic,/v1/messages,claude-haiku-4-5,json",
                &token_spec,
            ],
        ))
        .options(options(&f, on_gateway(&["*"])))
        .name("ceiling")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    said(
        &events,
        &[
            "call claude-sonnet-4-6 json: 403",
            "call claude-haiku-4-5 json: 200",
        ],
    );
    let narrowed = events
        .iter()
        .find_map(|e| match &e.activity {
            Activity::Access(a) => Some(a.as_ref().clone()),
            _ => None,
        })
        .expect("an access event");
    assert_eq!(narrowed.subject, subject);
    assert_eq!(narrowed.narrowed, ["models * to claude-haiku-*"]);
    let token = std::fs::read_to_string(&token_file).unwrap();
    let jwks: Value = serde_json::from_str(
        &std::fs::read_to_string(f.root.join(".branchyard/gateway/jwks.json")).unwrap(),
    )
    .unwrap();
    let claims = branchyard::connectors::keys::verify(&jwks, &token).unwrap();
    assert_eq!(claims["by_models"], json!(["claude-haiku-*"]));
    // The stored request is what the person asked for; the ceiling
    // applies to each turn.
    let stored = common::stored_record(&f.root, "ceiling");
    assert_eq!(stored["provision"]["models"]["allow"], json!(["*"]));
}

#[test]
fn a_confined_harness_reaches_its_gateway_and_nothing_else() {
    let why = branchyard_runtime::LocalProvider::confinement().err();
    if let Some(why) = why {
        let unshare = std::process::Command::new("unshare")
            .args(["-rn", "true"])
            .status();
        if unshare.is_ok_and(|s| s.success()) {
            panic!("unshare -rn works on this host but confinement does not: {why}");
        }
        eprintln!("SKIPPED a_confined_harness_reaches_its_gateway_and_nothing_else: {why}");
        return;
    }
    let f = Fixture::new();
    let anthropic = mock(Behavior::Answer);
    use_gateway(
        &f,
        json!({"backends": {"anthropic": {"api": "anthropic", "url": anthropic.url(),
            "key": "anthropic"}}}),
    );
    let script = harness(&f.dir);
    let mut provision = on_gateway(&["*"]);
    provision.network = Some(Network::none().with_enforce(NetworkEnforce::Required));
    // Through the proxy to the gateway; then straight at the upstream,
    // which the policy does not allow.
    let direct = format!(
        "SH python3 -c 'import socket; socket.create_connection((\"127.0.0.1\", {}), timeout=5)' \
         && echo upstream reached || echo upstream blocked",
        anthropic.port
    );
    let branch = f
        .task(&format!(
            "{}\n{direct}",
            prompt(
                &script,
                "proxy",
                &["anthropic,/v1/messages,claude-sonnet-4-6,stream"]
            )
        ))
        .options(options(&f, provision))
        .name("confined")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    said(
        &events,
        &[
            "real keys in env: none",
            "call claude-sonnet-4-6 stream: 200 events=4",
            "upstream blocked",
        ],
    );
    let gateway_port = events
        .iter()
        .find_map(|e| match &e.activity {
            Activity::Model(m) => match m.as_ref() {
                ModelActivity::Gateway { url, .. } => {
                    url.rsplit(':').next().and_then(|p| p.parse::<u16>().ok())
                }
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    let mut applied = None;
    let mut decisions = Vec::new();
    for event in &events {
        if let Activity::Egress(egress) = &event.activity {
            match egress.as_ref() {
                branchyard::EgressActivity::Applied {
                    allow, enforcement, ..
                } => applied = Some((allow.clone(), *enforcement)),
                branchyard::EgressActivity::Decision { port, allowed, .. } => {
                    decisions.push((*port, *allowed))
                }
            }
        }
    }
    // The gateway was added to the policy for this turn, and enforced.
    assert_eq!(
        applied,
        Some((
            vec![format!("127.0.0.1:{gateway_port}")],
            branchyard::EgressEnforcement::Enforced
        ))
    );
    assert_eq!(decisions, [(gateway_port, true)]);
    assert_eq!(anthropic.seen().len(), 1);
    assert_eq!(calls(&events)[0].decision, "allowed");
    // The stored policy is as it was set.
    let stored = common::stored_record(&f.root, "confined");
    assert_eq!(stored["provision"]["network"]["allow"], json!([]));
}

#[test]
fn a_delegated_child_stays_on_the_gateway_within_its_parents_models() {
    use branchyard::{ChildBudget, Error, Seat, Seats, Spawn};
    let f = Fixture::new();
    let anthropic = mock(Behavior::Answer);
    use_gateway(
        &f,
        json!({"backends": {"anthropic": {"api": "anthropic", "url": anthropic.url(),
            "key": "anthropic"}}}),
    );
    let seat = |models: Option<&[&str]>| Seat {
        harness: "gemini-cli".into(),
        budget: ChildBudget::default(),
        check: None,
        deny: Vec::new(),
        isolated: false,
        provision: Some(Provisioning {
            models: models.map(|m| models::ModelAccess {
                allow: m.iter().map(|p| (*p).to_owned()).collect(),
            }),
            ..Provisioning::default()
        }),
        delegates_to: Vec::new(),
        escalates_to: Vec::new(),
        instances: 2,
        bindings: Vec::new(),
    };
    let seats = Seats {
        rig: "team".into(),
        seat: "lead".into(),
        delegates_to: vec!["sonnet".into(), "plain".into(), "gpt".into()],
        escalates_to: Vec::new(),
        table: [
            ("sonnet".to_owned(), seat(Some(&["claude-sonnet-*"]))),
            ("plain".to_owned(), seat(None)),
            ("gpt".to_owned(), seat(Some(&["gpt-*"]))),
        ]
        .into_iter()
        .collect(),
    };
    let options = TaskOptions {
        delegation: Some(seats.envelope()),
        delegation_server: Some(vec![common::fake_agent().display().to_string()]),
        seats: Some(seats),
        ..options(&f, on_gateway(&["claude-*"]))
    };
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("lead")
        .run()
        .unwrap();
    let lead = root.delegate(options).unwrap();
    let by_seat = |seat: &str, name: &str| Spawn {
        seat: Some(seat.into()),
        name: Some(name.into()),
        ..Spawn::new("say hi")
    };
    lead.spawn(by_seat("sonnet", "s")).unwrap();
    // A seat that names no models keeps its parent's: still on the gateway.
    lead.spawn(by_seat("plain", "p")).unwrap();
    match lead.spawn(by_seat("gpt", "g")) {
        Err(Error::Denied(why)) if why.contains("gpt-*") => {}
        other => panic!("expected a denial naming gpt-*, got {other:?}"),
    }
    root.wait_subtree().unwrap();
    let allowed =
        |name: &str| common::stored_record(&f.root, name)["provision"]["models"]["allow"].clone();
    assert_eq!(allowed("s"), json!(["claude-sonnet-*"]));
    assert_eq!(allowed("p"), json!(["claude-*"]));
    assert!(f.yard.branch("g").is_err());
    // Each child's turn ran on a gateway of its own.
    for name in ["s", "p"] {
        let events = f.yard.branch(name).unwrap().events().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(&e.activity, Activity::Model(m)
                if matches!(m.as_ref(), ModelActivity::Gateway { .. }))),
            "{name} ran off the gateway"
        );
    }
}

/// A route naming a `backend_model` carries a Claude Code request to an
/// OpenAI backend: the request translated to a chat completion for that
/// model, the answer (streamed or not) translated back to Anthropic's
/// shape, usage read from the backend's and priced by its model.
#[test]
fn a_claude_request_is_translated_for_an_openai_backend() {
    let f = Fixture::new();
    let openai = mock(Behavior::Answer);
    use_gateway(
        &f,
        json!({
            "backends": {"openai": {"api": "openai", "url": openai.url(), "key": "openai"}},
            "routes": [{"model": "claude-*", "backends": ["openai"], "backend_model": "gpt-5"}]
        }),
    );
    let script = harness(&f.dir);
    let branch = f
        .task(&prompt(
            &script,
            "direct",
            &[
                "anthropic,/v1/messages,claude-sonnet-4-6,json",
                "anthropic,/v1/messages,claude-sonnet-4-6,stream",
                "anthropic,/v1/messages/count_tokens,claude-sonnet-4-6,json",
            ],
        ))
        .options(options(&f, on_gateway(&["claude-sonnet-*"])))
        .name("translated")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    said(
        &events,
        &[
            "real keys in env: none",
            "call claude-sonnet-4-6 json: 200 events=0 text=hello",
            "call claude-sonnet-4-6 stream: 200 events=6",
            "call claude-sonnet-4-6 json: 502 events=0",
        ],
    );
    // The gateway says it serves Anthropic's API, through the route.
    let served = events
        .iter()
        .find_map(|e| match &e.activity {
            Activity::Model(m) => match m.as_ref() {
                ModelActivity::Gateway { apis, .. } => Some(apis.clone()),
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    assert!(served.contains(&models::Api::Anthropic), "{served:?}");
    // The backend saw chat completions for its model, with the real key
    // and none of Anthropic's headers; the stream asked for its usage.
    let seen = openai.seen();
    assert_eq!(seen.len(), 2, "{seen:?}");
    for request in &seen {
        assert_eq!(request.path, "/v1/chat/completions");
        let bearer = format!("Bearer {OPENAI_KEY}");
        assert_eq!(request.header("authorization"), Some(bearer.as_str()));
        assert_eq!(request.header("anthropic-version"), None);
        assert_eq!(request.body["model"], "gpt-5");
        assert_eq!(
            request.body["messages"],
            json!([{"role": "user", "content": "hi"}])
        );
        assert_eq!(request.body["max_completion_tokens"], 16);
    }
    assert_eq!(seen[0].body.get("stream"), None);
    assert_eq!(seen[1].body["stream"], true);
    assert_eq!(seen[1].body["stream_options"]["include_usage"], true);
    // Recorded as the harness's model, translated, with the backend's
    // usage (cached prompt tokens as cache reads) priced as gpt-5.
    let recorded = calls(&events);
    assert_eq!(recorded.len(), 3, "{recorded:?}");
    for call in &recorded[..2] {
        assert_eq!(
            (call.decision.as_str(), call.status, call.api),
            ("allowed", 200, models::Api::Anthropic)
        );
        assert_eq!(call.model, "claude-sonnet-4-6");
        assert_eq!(call.backend.as_deref(), Some("openai"));
        assert_eq!(call.translated.as_deref(), Some("gpt-5"));
        assert_eq!(
            call.tokens,
            Some(models::Tokens {
                input: 40,
                output: 20,
                cache_read: 60,
                ..models::Tokens::default()
            })
        );
        assert!(call.cost_usd.is_some_and(|c| c > 0.0), "{call:?}");
    }
    assert_eq!(
        recorded.iter().map(|c| c.streamed).collect::<Vec<_>>(),
        [false, true, false]
    );
    // What the route cannot translate is refused, not sent anywhere.
    let refused = &recorded[2];
    assert_eq!((refused.decision.as_str(), refused.status), ("failed", 502));
    assert!(
        refused
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("count_tokens is not translated")),
        "{refused:?}"
    );
    // Usage rows carry the model the money went to.
    let rows = f.yard.model_usage(0).unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows
        .iter()
        .all(|r| r.model == "gpt-5" && r.api == models::Api::Openai && r.backend == "openai"));
    assert!(
        (branch.info().cost_usd.unwrap_or(0.0)
            - recorded.iter().filter_map(|c| c.cost_usd).sum::<f64>())
        .abs()
            < 1e-9
    );
}

fn digest_of(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// A scratch directory for the harness's `sent=` and its path as a spec.
fn sent_dir(f: &Fixture) -> (PathBuf, String) {
    let dir = f.dir.join("sent");
    std::fs::create_dir_all(&dir).unwrap();
    let spec = format!("sent={}", dir.display());
    (dir, spec)
}

/// What the harness sent as its `n`th call (from 1) and the answer it
/// read.
fn exchange(dir: &Path, n: usize) -> (Vec<u8>, String) {
    (
        std::fs::read(dir.join(n.to_string())).unwrap(),
        std::fs::read_to_string(dir.join(format!("{n}.response"))).unwrap(),
    )
}

/// The interception record of each call: the tools offered and the tools
/// called, in each API, streamed and not; the stop reason in the
/// provider's word; sizes; and a digest of the request as the harness sent
/// it, the same for the same request. Digests only: nothing is written.
#[test]
fn an_interception_records_the_tools_offered_and_called_in_each_api() {
    let f = Fixture::new();
    let (anthropic, openai) = (mock(Behavior::Answer), mock(Behavior::Answer));
    use_gateway(
        &f,
        json!({
            "audit": "digests",
            "backends": {
                "anthropic": {"api": "anthropic", "url": anthropic.url(), "key": "anthropic"},
                "openai": {"api": "openai", "url": openai.url(), "key": "openai"}
            }
        }),
    );
    let script = harness(&f.dir);
    let (sent, sent_spec) = sent_dir(&f);
    let branch = f
        .task(&prompt(
            &script,
            "direct",
            &[
                &sent_spec,
                "anthropic,/v1/messages,claude-sonnet-4-6,tools",
                "anthropic,/v1/messages,claude-sonnet-4-6,toolstream",
                "openai,/chat/completions,gpt-5,tools",
                "openai,/chat/completions,gpt-5,toolstream",
                "openai,/responses,gpt-5,tools",
                "openai,/responses,gpt-5,toolstream",
                "anthropic,/v1/messages,claude-sonnet-4-6,json",
                "anthropic,/v1/messages,claude-sonnet-4-6,json",
                "openai,/chat/completions,gpt-5,stream",
            ],
        ))
        .options(options(&f, on_gateway(&["claude-sonnet-*", "gpt-5"])))
        .name("intercepted")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    said(
        &events,
        &[
            "call claude-sonnet-4-6 tools: 200 events=0",
            "call claude-sonnet-4-6 toolstream: 200 events=7",
            "call gpt-5 tools: 200 events=0",
            "call gpt-5 toolstream: 200 events=6",
            "call gpt-5 toolstream: 200 events=5",
            "call gpt-5 stream: 200 events=3",
        ],
    );
    let recorded = calls(&events);
    assert_eq!(recorded.len(), 9, "{recorded:?}");
    assert!(recorded.iter().all(|c| c.decision == "allowed"));
    let seen: Vec<_> = recorded
        .iter()
        .map(|c| c.interception.clone().expect("an interception"))
        .collect();
    // The tools offered, and the tools the answer called, in each API; the
    // stop reason is Anthropic's `stop_reason`, chat's `finish_reason` and
    // the responses API's `status`.
    let stops = [
        "tool_use",
        "tool_use",
        "tool_calls",
        "tool_calls",
        "completed",
        "completed",
    ];
    for (i, stop) in stops.iter().enumerate() {
        assert_eq!(seen[i].tools_offered, ["Read", "Edit", "Bash"], "call {i}");
        assert_eq!(seen[i].tool_uses, ["Read", "Edit"], "call {i}");
        assert_eq!(seen[i].stop_reason.as_deref(), Some(*stop), "call {i}");
        assert_eq!(seen[i].messages, 1, "call {i}");
    }
    // Answers that call nothing, to requests that offered nothing.
    for (i, plain) in seen.iter().enumerate().skip(6) {
        assert!(plain.tools_offered.is_empty(), "call {i}");
        assert!(plain.tool_uses.is_empty(), "call {i}");
        assert_eq!(plain.stop_reason, None, "call {i}");
    }
    // Sizes: the request the harness sent, the answer it read (the backend's
    // bytes, which are the same ones when nothing is translated).
    for (i, call) in recorded.iter().enumerate() {
        let (payload, answer) = exchange(&sent, i + 1);
        let record = call.interception.as_ref().unwrap();
        assert_eq!(record.request_bytes, payload.len() as u64, "call {i}");
        assert_eq!(record.response_bytes, answer.len() as u64, "call {i}");
        // The digest is the BLAKE3 hash of the request body as sent.
        assert_eq!(record.prompt_digest, digest_of(&payload), "call {i}");
        assert_eq!(call.audit_file, None);
    }
    // The same request, the same digest; another request, another.
    assert_eq!(seen[6].prompt_digest, seen[7].prompt_digest);
    let digests: std::collections::BTreeSet<_> = seen.iter().map(|s| &s.prompt_digest).collect();
    assert_eq!(digests.len(), 8);
    // What the gateway changed on the way is not in the digest: it asked
    // the chat stream for its usage, so the backend saw other bytes.
    let forwarded = openai.seen();
    assert_eq!(forwarded.len(), 5);
    assert_eq!(
        forwarded[0].raw,
        exchange(&sent, 3).0,
        "a chat call is unchanged"
    );
    assert_eq!(forwarded[1].body["stream_options"]["include_usage"], true);
    assert_ne!(digest_of(&forwarded[1].raw), seen[3].prompt_digest);
    assert_eq!(digest_of(&exchange(&sent, 4).0), seen[3].prompt_digest);
    assert_eq!(forwarded[4].body["stream_options"]["include_usage"], true);
    assert_ne!(digest_of(&forwarded[4].raw), seen[8].prompt_digest);
    // The upstream got what offering tools meant, and the record is on the
    // branch's events, in `by log`'s words.
    assert_eq!(anthropic.seen()[0].body["tools"][2]["name"], "Bash");
    let line = recorded[1].describe();
    assert!(
        line.contains("tools: Read, Edit, stop: tool_use,"),
        "{line}"
    );
    // Digests keep no text: nothing is written, and the prompt is nowhere
    // in the branch's events.
    assert!(!f.root.join(".branchyard/models/audit").exists());
    let log = serde_json::to_string(&events).unwrap();
    assert!(!log.contains(MARKER), "the prompt was recorded");
}

/// `[models] audit = "contents"` appends one JSON line per forwarded call to
/// the turn's file, a private file in a private directory, and names it on
/// the call; the event log never holds the text; a refused call writes
/// nothing; a value other than `digests` or `contents` is refused by name.
#[test]
fn contents_are_written_to_a_private_file_only_when_asked_for() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let f = Fixture::new();
    let (anthropic, openai) = (mock(Behavior::Answer), mock(Behavior::Answer));
    use_gateway(
        &f,
        json!({
            "audit": "contents",
            "backends": {
                "anthropic": {"api": "anthropic", "url": anthropic.url(), "key": "anthropic"},
                "openai": {"api": "openai", "url": openai.url(), "key": "openai"}
            }
        }),
    );
    let script = harness(&f.dir);
    let (sent, sent_spec) = sent_dir(&f);
    let branch = f
        .task(&prompt(
            &script,
            "direct",
            &[
                &sent_spec,
                "anthropic,/v1/messages,claude-sonnet-4-6,tools",
                "anthropic,/v1/messages,claude-sonnet-4-6,toolstream",
                "openai,/chat/completions,gpt-5,toolstream",
                "anthropic,/v1/messages,claude-haiku-4-5,json",
            ],
        ))
        .options(options(&f, on_gateway(&["claude-sonnet-*", "gpt-5"])))
        .name("audited")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    said(
        &events,
        &["call claude-haiku-4-5 json: 403 events=0 permission_error"],
    );
    let recorded = calls(&events);
    assert_eq!(
        recorded
            .iter()
            .map(|c| c.decision.as_str())
            .collect::<Vec<_>>(),
        ["allowed", "allowed", "allowed", "denied"]
    );
    // The file is the turn's, 0600 in directories made 0700.
    let relative = ".branchyard/models/audit/audited/1.jsonl";
    let file = f.root.join(relative);
    assert_eq!(mode(&file), 0o600);
    for dir in ["models", "models/audit", "models/audit/audited"] {
        let dir = f.root.join(".branchyard").join(dir);
        assert_eq!(mode(&dir), 0o700, "{}", dir.display());
    }
    // The calls that were forwarded name it; the refused one writes none.
    for call in &recorded[..3] {
        assert_eq!(call.audit_file.as_deref(), Some(relative), "{call:?}");
        assert!(call.interception.is_some());
    }
    assert_eq!(
        (&recorded[3].audit_file, &recorded[3].interception),
        (&None, &None)
    );
    let text = std::fs::read_to_string(&file).unwrap();
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3, "{text}");
    let mut at = 0;
    for (i, line) in lines.iter().enumerate() {
        let keys: std::collections::BTreeSet<&str> = line
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            std::collections::BTreeSet::from(["at_ms", "backend", "model", "request", "response"]),
            "line {i}"
        );
        // The request is what the harness sent, not what the gateway forwarded.
        let (payload, answer) = exchange(&sent, i + 1);
        assert_eq!(
            line["request"],
            serde_json::from_slice::<Value>(&payload).unwrap()
        );
        assert!(line["request"].to_string().contains(MARKER), "line {i}");
        assert!(line["at_ms"].as_u64().unwrap() >= at);
        at = line["at_ms"].as_u64().unwrap();
        assert_eq!(line["model"], recorded[i].model);
        assert_eq!(line["backend"], recorded[i].backend.clone().unwrap());
        // Neither shape is rewritten: a JSON answer is its JSON, a stream
        // its raw text.
        match i {
            0 => assert_eq!(
                line["response"],
                serde_json::from_str::<Value>(&answer).unwrap()
            ),
            _ => assert_eq!(line["response"], answer.as_str()),
        }
    }
    assert_eq!(lines[0]["response"]["content"][1]["name"], "Read");
    assert!(lines[1]["response"]
        .as_str()
        .unwrap()
        .starts_with("event: message_start\n"));
    assert!(lines[2]["response"]
        .as_str()
        .unwrap()
        .contains("data: [DONE]"));
    assert_eq!(lines[2]["request"].get("stream_options"), None);
    // The text is in that file and nowhere in the branch's events or usage.
    let log = serde_json::to_string(&events).unwrap();
    assert!(!log.contains(MARKER), "the prompt was recorded");
    assert!(
        !log.contains("event: message_start"),
        "a stream was recorded"
    );
    assert!(!format!("{:?}", f.yard.model_usage(0).unwrap()).contains(MARKER));
    // The digests are there beside the contents.
    let (payload, _) = exchange(&sent, 1);
    assert_eq!(
        recorded[0].interception.as_ref().unwrap().prompt_digest,
        digest_of(&payload)
    );
    // Only `digests` and `contents` are values.
    for bad in ["everything", "Digests", ""] {
        let error = serde_json::from_value::<models::Config>(json!({"audit": bad}))
            .unwrap_err()
            .to_string();
        for needle in [format!("`{bad}`"), "`digests`".into(), "`contents`".into()] {
            assert!(error.contains(&needle), "{bad:?}: {error}");
        }
    }
}

/// A call translated for another API is read in the backend's shape: the
/// tools offered are the harness's (it sent them), the tools called and the
/// stop reason are the chat completion's, and the digest is of the
/// harness's request, not the translated one.
#[test]
fn a_translated_call_is_read_in_the_backends_shape() {
    let f = Fixture::new();
    let openai = mock(Behavior::Answer);
    use_gateway(
        &f,
        json!({
            "backends": {"openai": {"api": "openai", "url": openai.url(), "key": "openai"}},
            "routes": [{"model": "claude-*", "backends": ["openai"], "backend_model": "gpt-5"}]
        }),
    );
    let script = harness(&f.dir);
    let (sent, sent_spec) = sent_dir(&f);
    let branch = f
        .task(&prompt(
            &script,
            "direct",
            &[
                &sent_spec,
                "anthropic,/v1/messages,claude-sonnet-4-6,tools",
                "anthropic,/v1/messages,claude-sonnet-4-6,toolstream",
            ],
        ))
        .options(options(&f, on_gateway(&["claude-sonnet-*"])))
        .name("bridged")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    said(
        &events,
        &[
            "call claude-sonnet-4-6 tools: 200 events=0",
            "call claude-sonnet-4-6 toolstream: 200",
        ],
    );
    let recorded = calls(&events);
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    let forwarded = openai.seen();
    assert_eq!(forwarded.len(), 2);
    for (i, call) in recorded.iter().enumerate() {
        assert_eq!(call.translated.as_deref(), Some("gpt-5"));
        let seen = call.interception.as_ref().unwrap();
        // What the harness offered, in its own request's words.
        assert_eq!(seen.tools_offered, ["Read", "Edit", "Bash"], "call {i}");
        assert_eq!(seen.messages, 1);
        // What the backend called, and why it stopped, in its words: chat
        // completions say `tool_calls`, Anthropic says `tool_use`.
        assert_eq!(seen.tool_uses, ["Read", "Edit"], "call {i}");
        assert_eq!(seen.stop_reason.as_deref(), Some("tool_calls"), "call {i}");
        // The digest and size are of the Anthropic request as sent; the
        // backend was sent a chat completion.
        let (payload, answer) = exchange(&sent, i + 1);
        assert_eq!(seen.prompt_digest, digest_of(&payload), "call {i}");
        assert_eq!(seen.request_bytes, payload.len() as u64);
        assert_ne!(digest_of(&forwarded[i].raw), seen.prompt_digest, "call {i}");
        assert_eq!(forwarded[i].path, "/v1/chat/completions");
        // The response is the backend's, not the Anthropic message the
        // harness read in its place.
        assert!(seen.response_bytes > 0);
        assert_ne!(seen.response_bytes, answer.len() as u64, "call {i}");
    }
    // The harness read tool_use in Anthropic's shape.
    let (_, message) = exchange(&sent, 1);
    let message: Value = serde_json::from_str(&message).unwrap();
    assert_eq!(message["stop_reason"], "tool_use");
}

/// A call that would start with the branch's spend already at its limit is
/// refused before it is forwarded, not only one that starts over it: a
/// branch limited to exactly what one call costs makes that call, and the
/// ones after it are refused. Nothing was forwarded for them, the cost is
/// exactly the one call, and the turn ends over budget, stopped as the
/// call's end took it to the limit.
#[test]
fn a_call_that_would_start_at_the_branchs_limit_is_refused() {
    let f = Fixture::new();
    let upstream = mock(Behavior::Answer);
    use_gateway(
        &f,
        json!({"backends": {"anthropic": {"api": "anthropic", "url": upstream.url(),
            "key": "anthropic"}}}),
    );
    let script = harness(&f.dir);
    let one_call = "anthropic,/v1/messages,claude-sonnet-4-6,json";
    let probe = f
        .task(&prompt(&script, "direct", &[one_call]))
        .options(options(&f, on_gateway(&["*"])))
        .name("probe")
        .run()
        .unwrap();
    let one = probe.info().cost_usd.unwrap();
    assert!(one > 0.0);
    let mut opts = options(&f, on_gateway(&["*"]));
    opts.budget.max_usd = Some(one);
    let branch = f
        .task(&prompt(&script, "direct", &[one_call, one_call, one_call]))
        .options(opts)
        .name("exact")
        .run()
        .unwrap();
    let decisions: Vec<String> = calls(&branch.events().unwrap())
        .iter()
        .map(|c| c.decision.clone())
        .collect();
    assert_eq!(decisions, ["allowed", "budget", "budget"]);
    // The probe's call, and the first of this branch's.
    assert_eq!(upstream.seen().len(), 2);
    assert_eq!(branch.info().cost_usd, Some(one));
    assert_eq!(
        branch.info().status,
        branchyard::BranchStatus::BudgetExceeded {
            limit: "max_usd".into()
        }
    );
}

/// A cost limit above a branch bounds its calls through the gateway, which
/// counts what the whole tree under that limit has spent: the child was
/// given $0.0005 but the tree has only $0.0003 left (its parent spent the
/// rest itself), so its calls stop at the one that takes the tree to the
/// parent's limit, though its own is far off.
#[test]
fn a_childs_calls_stop_at_the_limit_its_tree_shares() {
    use branchyard::{Envelope, Spawn};
    let f = Fixture::new();
    let anthropic = mock(Behavior::Answer);
    use_gateway(
        &f,
        json!({"backends": {"anthropic": {"api": "anthropic", "url": anthropic.url(),
            "key": "anthropic"}}}),
    );
    let script = harness(&f.dir);
    // Haiku 4.5's call costs $0.0001025 (see the first test).
    let options = TaskOptions {
        budget: Budget {
            max_usd: Some(0.0006),
            ..Budget::default()
        },
        delegation: Some(Envelope::default()),
        delegation_server: Some(vec![common::fake_agent().display().to_string()]),
        ..options(&f, on_gateway(&["claude-*"]))
    };
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("parent")
        .run()
        .unwrap();
    let delegate = root.delegate(options).unwrap();
    // `waiter` ends at once, when the test says; the child that calls the
    // gateway waits for it, so the parent's spending is in place before.
    let go = f.dir.join("go");
    delegate
        .spawn(Spawn {
            prompt: format!("SH while [ ! -e {} ]; do sleep 0.05; done", go.display()),
            name: Some("waiter".into()),
            budget: Budget::usd(0.0001),
            ..Spawn::default()
        })
        .unwrap();
    let call = "anthropic,/v1/messages,claude-haiku-4-5,json";
    delegate
        .spawn(Spawn {
            prompt: prompt(&script, "direct", &[call, call, call, call]),
            name: Some("caller".into()),
            budget: Budget::usd(0.0005),
            depends_on: vec!["waiter".into()],
            ..Spawn::default()
        })
        .unwrap();
    // The parent spent $0.0003 of its $0.0006 by itself.
    common::edit_record(&f.root, "parent", |r| {
        r["info"]["cost_usd"] = 0.0003.into();
    });
    std::fs::write(&go, "").unwrap();
    branchyard_testkit::wait::until("the caller to settle", || {
        let info = f.yard.branch("caller").ok()?.info().clone();
        (!matches!(
            info.status,
            branchyard::BranchStatus::Running | branchyard::BranchStatus::Waiting
        ))
        .then_some(())
    });
    // Three calls started under the limit; the fourth would start over it
    // and was refused, or never made as the engine stopped the turn first.
    let events = f.yard.branch("caller").unwrap().events().unwrap();
    let recorded = calls(&events);
    assert!((3..=4).contains(&recorded.len()), "{recorded:?}");
    assert!(recorded[..3].iter().all(|c| c.decision == "allowed"));
    if let Some(fourth) = recorded.get(3) {
        assert_eq!(fourth.decision, "budget");
        assert!(
            fourth
                .reason
                .as_deref()
                .unwrap()
                .contains("the cost limit above this branch is spent"),
            "{fourth:?}"
        );
    }
    assert_eq!(anthropic.seen().len(), 3);
    let rows: Vec<_> = f
        .yard
        .model_usage(0)
        .unwrap()
        .into_iter()
        .filter(|r| r.branch == "caller")
        .collect();
    assert_eq!(rows.len(), 3);
    let spent: f64 = rows.iter().filter_map(|r| r.cost_usd).sum();
    assert!(spent < 0.0005, "far from its own limit: {spent}");
    assert!(0.0003 + spent >= 0.0006, "the tree's limit: {spent}");
}
