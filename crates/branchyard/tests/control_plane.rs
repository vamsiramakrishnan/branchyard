//! A model gateway backend that is an AIPlex access point
//! (`control_plane = "aiplex"`), through the engine: a fake harness (a
//! Python script the fake ACP agent runs) calls through its turn's gateway,
//! and `MockHttp` servers play two AIPlex access points and one provider
//! reached directly, shaped as the integration contract's section 5 says
//! (`X-Aiplex-Request-Id` on every answer, `X-Aiplex-Error-Code` on a
//! refusal of AIPlex's own). The harness forges every attribution header,
//! in other letter cases, repeated and folded. Hermetic: nothing leaves
//! this host, and no real AIPlex runs.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use branchyard::models::{self, KeySource, ModelActivity, ModelCall};
use branchyard::{Activity, Budget, Policy, Provisioning, RecordedEvent, TaskOptions};
use branchyard_testkit::{MockHttp, Request, Response};
use common::{text, Fixture};
use serde_json::{json, Value};

/// The token the gateway calls AIPlex with, and a provider's key.
const AIPLEX_TOKEN: &str = "aiplex-real-token-1";
const PROVIDER_KEY: &str = "sk-real-anthropic-9";
/// The turn's trace context, as a server would set it.
const TRACE: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

/// The fake harness: `harness.py PLAN OUT`. `PLAN` is a JSON list of
/// calls, each `{label, api, path, model, stream, headers, fold, method}`
/// (`method` `GET` sends no body), sent
/// with `http.client` so a header can be repeated, in any case, or folded
/// (`fold`: a name and the values put on continuation lines); `OUT` gets,
/// by label, the status, `Retry-After`, `X-Aiplex-Error-Code`,
/// `WWW-Authenticate` and body the harness read.
const HARNESS: &str = r#"import http.client, json, os, sys, urllib.parse
plan = json.load(open(sys.argv[1]))
real = sorted(k for k, v in os.environ.items() if "real" in v and ("sk-" in v or "aiplex" in v))
print("real keys in env:", ",".join(real) or "none")
out = {}
for call in plan:
    if call["api"] == "anthropic":
        base, token = os.environ["ANTHROPIC_BASE_URL"], os.environ["ANTHROPIC_API_KEY"]
        headers = [("x-api-key", token), ("anthropic-version", "2023-06-01")]
        body = {"model": call["model"], "max_tokens": 16, "messages": [{"role": "user", "content": "hi"}]}
    else:
        base, token = os.environ["OPENAI_BASE_URL"], os.environ["OPENAI_API_KEY"]
        headers = [("Authorization", "Bearer " + token)]
        if call["path"].endswith("/responses"):
            body = {"model": call["model"], "input": "hi"}
        else:
            body = {"model": call["model"], "messages": [{"role": "user", "content": "hi"}]}
    if call.get("stream"):
        body["stream"] = True
    method = call.get("method", "POST")
    payload = json.dumps(body).encode() if method == "POST" else b""
    url = urllib.parse.urlsplit(base + call["path"])
    conn = http.client.HTTPConnection(url.hostname, url.port, timeout=60)
    conn.putrequest(method, url.path, skip_accept_encoding=True)
    for name, value in headers + [tuple(h) for h in call.get("headers", [])]:
        conn.putheader(name, value)
    if call.get("fold"):
        conn.putheader(*call["fold"])
    if payload:
        conn.putheader("Content-Type", "application/json")
        conn.putheader("Content-Length", str(len(payload)))
    conn.endheaders(payload or None)
    response = conn.getresponse()
    data = response.read().decode()
    out[call["label"]] = {"status": response.status, "body": data,
        "retry_after": response.getheader("Retry-After"),
        "error_code": response.getheader("X-Aiplex-Error-Code"),
        "www_authenticate": response.getheader("WWW-Authenticate")}
    print(f"call {call['label']}: {response.status}")
json.dump(out, open(sys.argv[2], "w"))
"#;

/// The refusals an AIPlex access point makes itself, by status.
fn code(status: u16) -> &'static str {
    match status {
        401 => "UNAUTHENTICATED",
        403 => "CALLER_NOT_ALLOWED",
        404 => "MODEL_NOT_FOUND",
        429 => "QUOTA_BUDGET_EXHAUSTED",
        _ => "QUOTA_UNAVAILABLE",
    }
}

/// An error body in the wire's own shape, which AIPlex keeps.
fn error_body(path: &str, status: u16, message: &str) -> Value {
    match path.ends_with("/v1/messages") {
        true => json!({"type": "error", "error": {"type": "rate_limit_error", "message": message}}),
        false => json!({"error": {"message": message, "type": "requests", "code": status}}),
    }
}

/// An AIPlex access point at `{url}/ap/ide`, named `name` in the request ids
/// it answers with. A model `refuse-NNN...` is refused by AIPlex itself with
/// status NNN and its code; `...relay-NNN` is the provider's own NNN,
/// relayed (no code); anything else is answered as the provider would, with
/// usage, streamed when asked.
fn aiplex(name: &'static str) -> MockHttp {
    let n = AtomicUsize::new(0);
    MockHttp::start(move |request| {
        let id = format!("{name}-req-{}", n.fetch_add(1, Ordering::SeqCst) + 1);
        if request.method == "GET" {
            return Response::json(200, &json!({"object": "list", "data": [{"id": "m"}]}))
                .header("X-Aiplex-Request-Id", &id);
        }
        let model = model_of(request);
        let status = |rest: &str| rest.get(..3).and_then(|s| s.parse::<u16>().ok());
        if let Some(status) = model.strip_prefix("refuse-").and_then(status) {
            let mut answer = Response::json(
                status,
                &error_body(&request.path, status, &format!("{}: refused", code(status))),
            )
            .header("X-Aiplex-Request-Id", &id)
            .header("X-Aiplex-Error-Code", code(status));
            if matches!(status, 429 | 503) {
                answer = answer.header("Retry-After", "7");
            }
            if status == 401 {
                answer = answer.header(
                    "WWW-Authenticate",
                    "Bearer resource_metadata=\"http://aiplex.test/.well-known/oauth\"",
                );
            }
            return answer;
        }
        if let Some(status) = model.split_once("relay-").and_then(|(_, s)| status(s)) {
            return Response::json(status, &error_body(&request.path, status, "provider"))
                .header("X-Aiplex-Request-Id", &id);
        }
        answer(request, &model).header("X-Aiplex-Request-Id", &id)
    })
}

/// The model a request's body names; empty for one without a body.
fn model_of(request: &Request) -> String {
    serde_json::from_slice::<Value>(&request.body)
        .ok()
        .and_then(|b| b["model"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// A provider's answer to `request`, with usage.
fn answer(request: &Request, model: &str) -> Response {
    let stream = request.json()["stream"] == json!(true);
    let sse = |events: Vec<Value>| {
        let text: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        Response::new(200, text).header("Content-Type", "text/event-stream")
    };
    match (request.path.rsplit('/').next().unwrap_or(""), stream) {
        ("messages", false) => Response::json(
            200,
            &json!({"type": "message", "model": model,
                "content": [{"type": "text", "text": "hello"}],
                "usage": {"input_tokens": 10, "output_tokens": 5}}),
        ),
        ("messages", true) => sse(vec![
            json!({"type": "message_start", "message": {"model": model,
                "usage": {"input_tokens": 10, "output_tokens": 1}}}),
            json!({"type": "message_delta", "usage": {"output_tokens": 5}}),
            json!({"type": "message_stop"}),
        ]),
        ("completions", false) => Response::json(
            200,
            &json!({"model": model, "choices": [{"message": {"content": "hello"},
                "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5}}),
        ),
        ("completions", true) => sse(vec![
            json!({"model": model, "choices": [{"delta": {"content": "hello"}}]}),
            json!({"model": model, "choices": [],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5}}),
        ]),
        _ => sse(vec![
            json!({"type": "response.completed", "response": {"model": model,
                "status": "completed", "usage": {"input_tokens": 10, "output_tokens": 5}}}),
        ]),
    }
}

/// Every attribution header a harness could forge, in other cases and
/// twice, and its own credential where the gateway would not read it.
fn forged(api: &str) -> Value {
    let credential = match api {
        "anthropic" => json!(["Authorization", "Bearer forged-aiplex-credential"]),
        _ => json!(["api-key", "forged-aiplex-credential"]),
    };
    json!([
        ["X-Client-Request-Id", "forged-1"],
        ["x-client-request-id", "forged-2"],
        [
            "X-AIPLEX-TAGS",
            "by_branch=forged,by_turn=99,by_harness=forged"
        ],
        ["x-aiplex-tags", "by_turn=98"],
        ["X-AIPlex-Other", "forged"],
        [
            "traceparent",
            "00-11111111111111111111111111111111-2222222222222222-01"
        ],
        [
            "TRACEPARENT",
            "00-33333333333333333333333333333333-4444444444444444-01"
        ],
        ["tracestate", "vendor=forged"],
        ["X-Request-Id", "forged-request"],
        credential
    ])
}

fn call(label: &str, api: &str, path: &str, model: &str, stream: bool) -> Value {
    json!({"label": label, "api": api, "path": path, "model": model, "stream": stream,
        "headers": forged(api)})
}

/// The AIPlex backends at `first` and `second`, a provider reached
/// directly at `direct`, and their routes.
fn config(first: &MockHttp, second: &MockHttp, direct: &MockHttp) -> Value {
    let plane = |api: &str, mock: &MockHttp| {
        json!({"api": api, "url": format!("{}/ap/ide", mock.url()), "key": "aiplex",
            "control_plane": "aiplex"})
    };
    json!({
        "backends": {
            "aiplex-anthropic": plane("anthropic", first),
            "aiplex-openai": plane("openai", first),
            "spare-anthropic": plane("anthropic", second),
            "direct": {"api": "anthropic", "url": direct.url(), "key": "anthropic"}
        },
        "routes": [
            {"model": "direct-*", "backends": ["direct"]},
            {"model": "claude-haiku-*", "backends": ["aiplex-openai"],
                "backend_model": "gpt-5-mini"},
            {"model": "claude-*", "backends": ["aiplex-anthropic", "spare-anthropic"]},
            {"model": "refuse-*", "backends": ["aiplex-anthropic", "spare-anthropic"],
                "weights": [1, 3]},
            {"model": "relay-*", "backends": ["aiplex-anthropic"],
                "fallbacks": ["spare-anthropic"]}
        ],
        "audit": "contents",
        "seed": 11
    })
}

fn use_gateway(f: &Fixture, config: Value) {
    std::fs::write(f.dir.join("aiplex"), format!("{AIPLEX_TOKEN}\n")).unwrap();
    std::fs::write(f.dir.join("anthropic"), PROVIDER_KEY).unwrap();
    let config: models::Config = serde_json::from_value(config).unwrap();
    let signer = models::Signer::local(&f.yard).unwrap();
    let dir = f.dir.clone();
    let gateway =
        models::Gateway::new(&config, signer, move |name| KeySource::File(dir.join(name))).unwrap();
    f.yard.use_models(gateway);
}

/// Run the plan as branch `name`, with `trace` as the turn's trace context:
/// the branch's events and what the harness read, by label.
fn run(
    f: &Fixture,
    name: &str,
    trace: Option<&str>,
    plan: &Value,
) -> (Vec<RecordedEvent>, BTreeMap<String, Value>) {
    let script = f.dir.join("harness.py");
    std::fs::write(&script, HARNESS).unwrap();
    let plan_file = f.dir.join(format!("{name}.plan.json"));
    let out_file = f.dir.join(format!("{name}.out.json"));
    std::fs::write(&plan_file, plan.to_string()).unwrap();
    let prompt = format!(
        "SH python3 {} {} {}",
        script.display(),
        plan_file.display(),
        out_file.display()
    );
    let options = TaskOptions {
        policy: Policy::allow_all(),
        provision: Some(Provisioning {
            models: Some(models::ModelAccess {
                allow: vec!["*".into()],
            }),
            ..Provisioning::default()
        }),
        budget: Budget {
            max_duration: Some(Duration::from_secs(120)),
            ..Budget::default()
        },
        trace_parent: trace.map(str::to_owned),
        ..f.options()
    };
    let branch = f.task(&prompt).options(options).name(name).run().unwrap();
    let events = branch.events().unwrap();
    let read = std::fs::read_to_string(&out_file);
    assert!(
        read.is_ok(),
        "the harness wrote no answers: {}",
        text(&events)
    );
    (events, serde_json::from_str(&read.unwrap()).unwrap())
}

fn calls(events: &[RecordedEvent]) -> BTreeMap<String, ModelCall> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Model(m) => match m.as_ref() {
                ModelActivity::Call(call) => Some((call.model.clone(), call.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The requests in `seen` for `model`.
fn asked<'a>(seen: &'a [Request], model: &str) -> Vec<&'a Request> {
    seen.iter().filter(|r| model_of(r) == model).collect()
}

/// Every file under `dir`, read.
fn read_all(dir: &Path) -> String {
    let mut all = String::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        match path.is_dir() {
            true => all.push_str(&read_all(&path)),
            false => all.push_str(&std::fs::read_to_string(&path).unwrap()),
        }
    }
    all
}

#[test]
fn a_control_plane_hears_only_the_gateway_and_its_refusal_is_final_on_every_path() {
    let f = Fixture::new();
    let (first, second, direct) = (aiplex("one"), aiplex("two"), aiplex("direct"));
    use_gateway(&f, config(&first, &second, &direct));
    let mut plan = vec![
        call(
            "anthropic",
            "anthropic",
            "/v1/messages",
            "claude-ok-json",
            false,
        ),
        call(
            "anthropic stream",
            "anthropic",
            "/v1/messages",
            "claude-ok-stream",
            true,
        ),
        call("chat", "openai", "/chat/completions", "gpt-ok-json", false),
        call(
            "chat stream",
            "openai",
            "/chat/completions",
            "gpt-ok-stream",
            true,
        ),
        call(
            "responses stream",
            "openai",
            "/responses",
            "gpt-ok-responses",
            true,
        ),
        // Translated for AIPlex's OpenAI wire.
        call(
            "translated",
            "anthropic",
            "/v1/messages",
            "claude-haiku-4-5",
            false,
        ),
        // AIPlex refuses: final, whatever the status.
        call(
            "refuse 401",
            "anthropic",
            "/v1/messages",
            "refuse-401",
            false,
        ),
        call(
            "refuse 403",
            "anthropic",
            "/v1/messages",
            "refuse-403",
            false,
        ),
        call(
            "refuse 404",
            "anthropic",
            "/v1/messages",
            "refuse-404",
            false,
        ),
        call(
            "refuse 429",
            "anthropic",
            "/v1/messages",
            "refuse-429",
            false,
        ),
        call(
            "refuse 503",
            "anthropic",
            "/v1/messages",
            "refuse-503",
            false,
        ),
        call(
            "refuse 429 streamed",
            "anthropic",
            "/v1/messages",
            "refuse-429-stream",
            true,
        ),
        // No route takes this one, so only AIPlex's OpenAI backend may.
        call(
            "refuse 503 unrouted",
            "openai",
            "/chat/completions",
            "refuse-503-o3",
            false,
        ),
        // The provider's own errors, relayed: the old rules, among AIPlex
        // backends only.
        call("relay 429", "anthropic", "/v1/messages", "relay-429", false),
        call("relay 503", "anthropic", "/v1/messages", "relay-503", false),
        call("relay 404", "anthropic", "/v1/messages", "relay-404", false),
        // No route, on an API with a plain backend: the plain one only, as
        // before; its 503 never moves the call to AIPlex.
        call(
            "relay 503 unrouted",
            "anthropic",
            "/v1/messages",
            "llama-relay-503",
            false,
        ),
        // A plain route, as before.
        call("direct", "anthropic", "/v1/messages", "direct-ok", false),
    ];
    // A folded attribution header is not read as anything: refused.
    let mut folded = call(
        "folded",
        "anthropic",
        "/v1/messages",
        "claude-folded",
        false,
    );
    folded["fold"] = json!(["X-AIPlex-Tags", "by_branch=forged", "by_turn=9"]);
    plan.push(folded);
    let (events, read) = run(&f, "cp", Some(TRACE), &json!(plan));
    assert!(
        text(&events).contains("real keys in env: none"),
        "{}",
        text(&events)
    );
    let seen: Vec<Request> = [first.requests(), second.requests()].concat();
    let calls = calls(&events);
    let rows = f.yard.model_usage(0).unwrap();

    // What AIPlex was sent: the gateway's word and nothing of the harness's.
    for request in &seen {
        let model = request.json()["model"].clone();
        let bearer = format!("Bearer {AIPLEX_TOKEN}");
        assert_eq!(
            request.header_values("authorization"),
            [bearer.as_str()],
            "{model}"
        );
        assert!(request.header_values("x-api-key").is_empty(), "{model}");
        assert!(request.header_values("api-key").is_empty(), "{model}");
        let id = request.header_values("X-Client-Request-Id");
        assert_eq!(id.len(), 1, "{model}: {id:?}");
        assert!(!id[0].starts_with("forged"), "{model}: {id:?}");
        assert_eq!(
            request.header_values("X-AIPlex-Tags"),
            ["by_branch=cp,by_turn=1,by_harness=gemini-cli"],
            "{model}"
        );
        assert_eq!(request.header_values("traceparent"), [TRACE], "{model}");
        for gone in ["tracestate", "x-request-id", "x-aiplex-other"] {
            assert!(request.header_values(gone).is_empty(), "{model}: {gone}");
        }
        // Spelled as the contract spells them.
        let names: Vec<&str> = request
            .sent_headers
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        for name in [
            "X-Client-Request-Id",
            "X-AIPlex-Tags",
            "traceparent",
            "Authorization",
        ] {
            assert!(names.contains(&name), "{model}: {names:?}");
        }
        assert!(
            !request
                .sent_headers
                .iter()
                .any(|(_, v)| v.contains("forged")),
            "{model}: {:?}",
            request.sent_headers
        );
    }
    // Each wire at its path under the access point.
    let path_of = |model: &str| asked(&seen, model)[0].path.clone();
    assert_eq!(path_of("claude-ok-json"), "/ap/ide/v1/messages");
    assert_eq!(path_of("gpt-ok-json"), "/ap/ide/v1/chat/completions");
    assert_eq!(path_of("gpt-ok-responses"), "/ap/ide/v1/responses");
    let translated = asked(&seen, "gpt-5-mini");
    assert_eq!(translated.len(), 1);
    assert_eq!(translated[0].path, "/ap/ide/v1/chat/completions");
    assert!(translated[0].header("anthropic-version").is_none());

    // Answered calls: the harness got AIPlex's answer, and the id it was
    // sent is the id of the call's usage record and is on its event.
    for (label, model) in [
        ("anthropic", "claude-ok-json"),
        ("anthropic stream", "claude-ok-stream"),
        ("chat", "gpt-ok-json"),
        ("chat stream", "gpt-ok-stream"),
        ("responses stream", "gpt-ok-responses"),
    ] {
        assert_eq!(read[label]["status"], 200, "{label}: {}", read[label]);
        let requests = asked(&seen, model);
        assert_eq!(requests.len(), 1, "{model}");
        let id = requests[0].header("X-Client-Request-Id").unwrap();
        let row: Vec<_> = rows.iter().filter(|r| r.model == model).collect();
        assert_eq!(row.len(), 1, "{model}");
        assert_eq!(row[0].id, id, "{model}");
        let call = &calls[model];
        assert_eq!(call.decision, "allowed", "{model}");
        let plane = call.control_plane.as_deref().unwrap();
        assert_eq!(plane.call_id, id, "{model}");
        let upstream = plane.request_id.as_deref().unwrap();
        // The access point that answered: either, by the weighted choice.
        let from = requests[0].path.clone();
        assert!(
            upstream.starts_with("one-req-") || upstream.starts_with("two-req-"),
            "{model}: {upstream} ({from})"
        );
        assert_eq!(plane.error_code, None);
        // Metered by Branchyard from the answer, as for any backend.
        assert!(call.tokens.is_some(), "{model}");
    }
    assert_eq!(read["translated"]["status"], 200, "{}", read["translated"]);

    // AIPlex's refusals: one request each, never retried, never moved, and
    // the harness gets status, body and Retry-After as AIPlex sent them.
    for (label, model, status) in [
        ("refuse 401", "refuse-401", 401),
        ("refuse 403", "refuse-403", 403),
        ("refuse 404", "refuse-404", 404),
        ("refuse 429", "refuse-429", 429),
        ("refuse 503", "refuse-503", 503),
        ("refuse 429 streamed", "refuse-429-stream", 429),
        ("refuse 503 unrouted", "refuse-503-o3", 503),
    ] {
        let got = &read[label];
        assert_eq!(got["status"], status, "{label}: {got}");
        let requests = asked(&seen, model);
        assert_eq!(requests.len(), 1, "{label}: tried {} times", requests.len());
        let path = requests[0].path.clone();
        let expected = error_body(&path, status, &format!("{}: refused", code(status)));
        assert_eq!(
            serde_json::from_str::<Value>(got["body"].as_str().unwrap()).unwrap(),
            expected,
            "{label}"
        );
        assert_eq!(got["error_code"], code(status), "{label}");
        let retry = match status {
            429 | 503 => json!("7"),
            _ => Value::Null,
        };
        assert_eq!(got["retry_after"], retry, "{label}");
        // AIPlex's challenge, which names its URL, is not passed on.
        assert_eq!(got["www_authenticate"], Value::Null, "{label}");
        let call = &calls[model];
        assert_eq!(call.decision, "refused", "{label}");
        assert_eq!(call.status, status);
        let plane = call.control_plane.as_deref().unwrap();
        assert_eq!(plane.error_code.as_deref(), Some(code(status)));
        assert!(plane.request_id.is_some(), "{label}");
        let reason = call.reason.as_deref().unwrap();
        assert!(
            reason.contains(&format!("refused by aiplex ({})", code(status))),
            "{label}: {reason}"
        );
        assert!(
            call.failed_over.is_empty(),
            "{label}: {:?}",
            call.failed_over
        );
        assert!(
            call.describe().contains("aiplex request "),
            "{}",
            call.describe()
        );
        // Stored under the id AIPlex was sent.
        let id = requests[0].header("X-Client-Request-Id").unwrap();
        assert!(
            rows.iter().any(|r| r.model == model && r.id == id),
            "{label}"
        );
    }

    // The provider's own errors, relayed: 429 and 503 move to the next
    // AIPlex backend with the same id, a 404 is the answer; none reaches
    // the provider directly.
    for (label, model, tries) in [
        ("relay 429", "relay-429", 2),
        ("relay 503", "relay-503", 2),
        ("relay 404", "relay-404", 1),
    ] {
        let requests = asked(&seen, model);
        assert_eq!(requests.len(), tries, "{label}");
        let ids: BTreeSet<&str> = requests
            .iter()
            .map(|r| r.header("X-Client-Request-Id").unwrap())
            .collect();
        assert_eq!(ids.len(), 1, "{label}: {ids:?}");
        let call = &calls[model];
        assert_eq!(call.decision, "failed", "{label}");
        let plane = call.control_plane.as_deref().unwrap();
        assert_eq!(plane.error_code, None, "{label}");
        assert_eq!(
            Some(plane.call_id.as_str()),
            ids.first().copied(),
            "{label}"
        );
        assert!(plane.request_id.is_some(), "{label}");
        assert_eq!(read[label]["error_code"], Value::Null, "{label}");
    }
    assert_eq!(calls["relay-503"].failed_over.len(), 1);
    assert!(
        calls["relay-503"].failed_over[0]
            .contains("aiplex-anthropic: 503 (aiplex request one-req-"),
        "{:?}",
        calls["relay-503"].failed_over
    );
    assert_eq!(read["relay 404"]["status"], 404);
    assert_eq!(read["relay 503 unrouted"]["status"], 503);
    assert!(asked(&seen, "llama-relay-503").is_empty());
    assert_eq!(calls["llama-relay-503"].backend.as_deref(), Some("direct"));
    assert_eq!(calls["llama-relay-503"].control_plane, None);

    // The folded header: refused by the gateway, sent nowhere.
    assert_eq!(read["folded"]["status"], 400, "{}", read["folded"]);
    assert!(asked(&seen, "claude-folded").is_empty());

    // The provider reached directly saw its own route's call and the
    // unrouted one, as it always has: the harness's headers and the provider's key, and none
    // of the gateway's attribution.
    let plain = direct.requests();
    assert_eq!(
        plain.iter().map(model_of).collect::<Vec<_>>(),
        ["llama-relay-503", "direct-ok"]
    );
    assert_eq!(read["direct"]["status"], 200);
    assert_eq!(plain[0].header_values("x-api-key"), [PROVIDER_KEY]);
    assert_eq!(
        plain[0].header_values("X-Client-Request-Id"),
        ["forged-1", "forged-2"]
    );
    assert_eq!(
        plain[0].header_values("X-AIPlex-Tags"),
        [
            "by_branch=forged,by_turn=99,by_harness=forged",
            "by_turn=98"
        ]
    );
    let call = &calls["direct-ok"];
    assert_eq!(call.decision, "allowed");
    assert_eq!(call.control_plane, None);

    // Neither credential is anywhere the branch's record or audit keeps.
    let recorded = serde_json::to_string(&events).unwrap();
    let audit = read_all(&f.root.join(".branchyard/models/audit"));
    assert!(
        audit.contains("claude-ok-json"),
        "the audit file holds the calls"
    );
    for secret in [AIPLEX_TOKEN, PROVIDER_KEY] {
        assert!(!recorded.contains(secret), "{secret} in the events");
        assert!(!audit.contains(secret), "{secret} in the audit file");
        assert!(
            !read.values().any(|r| r.to_string().contains(secret)),
            "{secret} reached the harness"
        );
    }
    first.finish();
    second.finish();
    direct.finish();
}

#[test]
fn a_branch_name_that_does_not_fit_a_tag_is_left_out_and_no_trace_is_made_up() {
    let f = Fixture::new();
    let (first, second, direct) = (aiplex("one"), aiplex("two"), aiplex("direct"));
    use_gateway(&f, config(&first, &second, &direct));
    // 65 characters: one more than a tag value may hold.
    let name = format!("b{}", "x".repeat(64));
    let plan = json!([call(
        "long",
        "anthropic",
        "/v1/messages",
        "claude-ok-json",
        false
    )]);
    let (events, read) = run(&f, &name, None, &plan);
    assert_eq!(read["long"]["status"], 200, "{}", text(&events));
    let seen = [first.requests(), second.requests()].concat();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].header_values("X-AIPlex-Tags"),
        ["by_turn=1,by_harness=gemini-cli"]
    );
    // The turn has no trace context: the harness's is dropped, none is sent.
    assert!(seen[0].header_values("traceparent").is_empty());
    assert_eq!(seen[0].header_values("X-Client-Request-Id").len(), 1);
    first.finish();
    second.finish();
    direct.finish();
}

#[test]
fn a_call_no_route_takes_goes_to_the_plain_backends_of_its_api_else_to_aiplex() {
    let f = Fixture::new();
    let access = aiplex("one");
    // Anthropic's API: a plain backend that refuses connections, and an
    // AIPlex backend a route names. OpenAI's: AIPlex only, named by none.
    let at = format!("{}/ap/ide", access.url());
    use_gateway(
        &f,
        json!({
            "backends": {
                "aiplex-anthropic": {"api": "anthropic", "url": at, "key": "aiplex",
                    "control_plane": "aiplex"},
                "aiplex-openai": {"api": "openai", "url": at, "key": "aiplex",
                    "control_plane": "aiplex"},
                "down": {"api": "anthropic", "url": "http://127.0.0.1:1", "key": "anthropic"}
            },
            "routes": [{"model": "claude-*", "backends": ["aiplex-anthropic"]}]
        }),
    );
    let get = |label: &str, api: &str| {
        // OPENAI_BASE_URL already ends in /v1; ANTHROPIC_BASE_URL does not.
        let path = if api == "openai" {
            "/models"
        } else {
            "/v1/models"
        };
        json!({"label": label, "api": api, "path": path, "model": "",
            "method": "GET", "headers": forged(api)})
    };
    let plan = json!([
        call("unrouted", "anthropic", "/v1/messages", "llama-3", false),
        get("models anthropic", "anthropic"),
        call(
            "routed",
            "anthropic",
            "/v1/messages",
            "claude-ok-json",
            false
        ),
        call(
            "unrouted openai",
            "openai",
            "/chat/completions",
            "o3-ok",
            false
        ),
        call(
            "refused openai",
            "openai",
            "/chat/completions",
            "refuse-429-o3",
            false
        ),
        get("models openai", "openai"),
    ]);
    let (events, read) = run(&f, "unrouted", Some(TRACE), &plan);
    let seen = access.requests();
    // The plain backend could not be reached: the call failed there, and
    // AIPlex heard nothing of it, nor of the model list.
    for label in ["unrouted", "models anthropic"] {
        assert_eq!(read[label]["status"], 502, "{label}: {}", read[label]);
    }
    assert!(asked(&seen, "llama-3").is_empty());
    assert!(
        !seen.iter().any(|r| r.method == "GET"
            && r.path == "/ap/ide/v1/models"
            && r.header("anthropic-version").is_some()),
        "{seen:?}"
    );
    let calls = calls(&events);
    assert!(calls["llama-3"]
        .reason
        .as_deref()
        .unwrap()
        .contains("down: could not connect"));
    // A route names AIPlex: it answers.
    assert_eq!(read["routed"]["status"], 200);
    // OpenAI's API has no plain backend: an unrouted call, and the model
    // list, go to AIPlex, and its refusal is final.
    assert_eq!(
        read["unrouted openai"]["status"], 200,
        "{}",
        read["unrouted openai"]
    );
    assert_eq!(asked(&seen, "o3-ok").len(), 1);
    assert_eq!(read["refused openai"]["status"], 429);
    assert_eq!(read["refused openai"]["retry_after"], "7");
    assert_eq!(asked(&seen, "refuse-429-o3").len(), 1);
    assert_eq!(calls["refuse-429-o3"].decision, "refused");
    assert_eq!(
        read["models openai"]["status"], 200,
        "{}",
        read["models openai"]
    );
    let listed: Vec<&Request> = seen.iter().filter(|r| r.method == "GET").collect();
    assert_eq!(listed.len(), 1, "{seen:?}");
    assert_eq!(listed[0].path, "/ap/ide/v1/models");
    let bearer = format!("Bearer {AIPLEX_TOKEN}");
    assert_eq!(listed[0].header_values("authorization"), [bearer.as_str()]);
    assert_eq!(listed[0].header_values("X-Client-Request-Id").len(), 1);
    assert!(!listed[0]
        .sent_headers
        .iter()
        .any(|(_, v)| v.contains("forged")));
    access.finish();
}
