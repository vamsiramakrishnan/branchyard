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
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let model = body["model"].as_str().unwrap_or("").to_owned();
    let stream = body["stream"] == json!(true);
    let usage_asked = body["stream_options"]["include_usage"] == json!(true);
    let _ = sender.send(Seen {
        path: path.clone(),
        headers,
        body,
    });
    if let Behavior::Status(code) = behavior {
        let body = format!(r#"{{"error":{{"type":"upstream_{code}","message":"mock"}}}}"#);
        let _ = write!(
            out,
            "HTTP/1.1 {code} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
        return;
    }
    let events: Vec<String> = match (path.as_str(), stream) {
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
        let _ = write!(
            out,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\
             Connection: close\r\n\r\n"
        );
        for event in events {
            let _ = write!(out, "{:x}\r\n{event}\r\n", event.len());
            let _ = out.flush();
        }
        let _ = write!(out, "0\r\n\r\n");
        return;
    }
    let body = match path.as_str() {
        "/v1/messages" => json!({"type": "message", "model": model,
            "content": [{"type": "text", "text": "hello"}],
            "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 100,
                "cache_creation_input_tokens": 40,
                "cache_creation": {"ephemeral_5m_input_tokens": 30, "ephemeral_1h_input_tokens": 10}}}),
        _ => json!({"model": model, "choices": [{"message": {"content": "hello"}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20,
                "prompt_tokens_details": {"cached_tokens": 60}}}),
    }
    .to_string();
    let _ = write!(
        out,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
}

/// The fake harness: `harness.py direct|proxy CALL...`. A call is
/// `API,PATH,MODEL,json|stream[,KEY]` (PATH after the base URL), sent the
/// way that provider's SDK sends it, printing `call MODEL MODE: STATUS
/// events=N [ERROR TYPE] [retry=SECONDS]`; `token=FILE` writes the
/// harness's ANTHROPIC_API_KEY to FILE. It first prints the names of any
/// variables holding a real key (`sk-real...`). `direct` ignores the proxy
/// variables; `proxy` honors them.
fn harness(dir: &Path) -> PathBuf {
    let path = dir.join("harness.py");
    std::fs::write(
        &path,
        r#"import json, os, sys, urllib.error, urllib.request
mode = sys.argv[1]
handlers = [] if mode == "proxy" else [urllib.request.ProxyHandler({})]
opener = urllib.request.build_opener(*handlers)
real = sorted(k for k, v in os.environ.items() if "sk-real" in v)
print("real keys in env:", ",".join(real) or "none")
for spec in sys.argv[2:]:
    if spec.startswith("token="):
        with open(spec[6:], "w") as f:
            f.write(os.environ.get("ANTHROPIC_API_KEY", ""))
        continue
    parts = spec.split(",")
    api, path, model, kind = parts[:4]
    if api == "anthropic":
        base = os.environ["ANTHROPIC_BASE_URL"]
        key = parts[4] if len(parts) > 4 else os.environ["ANTHROPIC_API_KEY"]
        headers = {"x-api-key": key, "anthropic-version": "2023-06-01"}
        body = {"model": model, "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]}
    else:
        base = os.environ["OPENAI_BASE_URL"]
        key = parts[4] if len(parts) > 4 else os.environ["OPENAI_API_KEY"]
        headers = {"Authorization": "Bearer " + key}
        if path.endswith("/responses"):
            body = {"model": model, "input": "hi"}
        else:
            body = {"model": model, "messages": [{"role": "user", "content": "hi"}]}
    if kind == "stream":
        body["stream"] = True
    headers["content-type"] = "application/json"
    payload = json.dumps(body).encode() if kind != "broken" else b'{"model": "' + model.encode()
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
    events = sum(1 for line in data.splitlines() if line.startswith("data:"))
    line = f"call {model} {kind}: {status} events={events}"
    if status >= 400:
        try:
            line += " " + json.loads(data)["error"]["type"]
        except Exception:
            line += " ?"
    if retry:
        line += f" retry={retry}"
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
