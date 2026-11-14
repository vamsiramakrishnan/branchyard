//! What the AIPlex tests share: a fake harness, `MockHttp` access points
//! shaped as the integration contract's section 5 says, and running a turn
//! through the gateway. See `tests/control_plane.rs`.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)] // tests: a panic is the failure report

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use branchyard::models::{self, KeySource, ModelActivity, ModelCall};
use branchyard::{Activity, Budget, Policy, Provisioning, RecordedEvent, TaskOptions};
use branchyard_testkit::{MockHttp, Request, Response};
use serde_json::{json, Value};

use crate::common::{text, Fixture};

/// The token the gateway calls AIPlex with, and a provider's key.
pub const AIPLEX_TOKEN: &str = "aiplex-real-token-1";
pub const PROVIDER_KEY: &str = "sk-real-anthropic-9";
/// The turn's trace context, as a server would set it.
pub const TRACE: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

/// The fake harness: `harness.py PLAN OUT`. `PLAN` is a JSON list of
/// calls, each `{label, api, path, model, stream, headers, fold, method}`
/// (`method` `GET` sends no body; `raw_path`, a path at the gateway's root
/// such as the pass-through `/backends/NAME/...`; a path is sent as
/// written, with its query and without normalizing it), sent
/// with `http.client` so a header can be repeated, in any case, or folded
/// (`fold`: a name and the values put on continuation lines); `OUT` gets,
/// by label, the status, every response header, `Retry-After`,
/// `X-Aiplex-Error-Code`, `WWW-Authenticate` and body the harness read. It
/// prints the variables of its environment holding a real key, and those
/// named `BY_TEST_*`. A call with `close_early` reads the status and closes
/// without reading the body, as a harness that goes away; `key` presents
/// that key instead of the turn's token (Anthropic's API); an entry
/// `{"dump_token": FILE}` writes the harness's token to `FILE`.
pub const HARNESS: &str = r#"import http.client, json, os, sys, urllib.parse
plan = json.load(open(sys.argv[1]))
real = sorted(k for k, v in os.environ.items() if "sk-real" in v or "aiplex-real" in v)
print("real keys in env:", ",".join(real) or "none")
print("test variables in env:", ",".join(sorted(k for k in os.environ if k.startswith("BY_TEST_"))) or "none")
out = {}
for call in plan:
    if call.get("dump_token"):
        with open(call["dump_token"], "w") as f:
            f.write(os.environ.get("ANTHROPIC_API_KEY", ""))
        continue
    if call["api"] == "anthropic":
        base, token = os.environ["ANTHROPIC_BASE_URL"], call.get("key") or os.environ["ANTHROPIC_API_KEY"]
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
    if call.get("raw_path"):
        base = os.environ["BRANCHYARD_MODEL_GATEWAY"]
        headers = [("x-api-key", token), ("anthropic-version", "2023-06-01")]
    url = urllib.parse.urlsplit(base + (call.get("raw_path") or call["path"]))
    conn = http.client.HTTPConnection(url.hostname, url.port, timeout=60)
    conn.putrequest(method, url.path + ("?" + url.query if url.query else ""), skip_accept_encoding=True)
    for name, value in headers + [tuple(h) for h in call.get("headers", [])]:
        conn.putheader(name, value)
    if call.get("fold"):
        conn.putheader(*call["fold"])
    if payload:
        conn.putheader("Content-Type", "application/json")
        conn.putheader("Content-Length", str(len(payload)))
    conn.endheaders(payload or None)
    response = conn.getresponse()
    if call.get("close_early"):
        # A harness that goes away: the status read, the body not.
        status = response.status
        response.close()
        conn.close()
        out[call["label"]] = {"status": status, "body": "", "headers": [],
            "retry_after": None, "error_code": None, "www_authenticate": None}
        print(f"call {call['label']}: {status} (closed early)")
        continue
    data = response.read().decode(errors="replace")
    out[call["label"]] = {"status": response.status, "body": data,
        "headers": [[n.lower(), v] for n, v in response.getheaders()],
        "retry_after": response.getheader("Retry-After"),
        "error_code": response.getheader("X-Aiplex-Error-Code"),
        "www_authenticate": response.getheader("WWW-Authenticate")}
    print(f"call {call['label']}: {response.status}")
json.dump(out, open(sys.argv[2], "w"))
"#;

/// The refusals an AIPlex access point makes itself, by status.
pub fn code(status: u16) -> &'static str {
    match status {
        401 => "UNAUTHENTICATED",
        403 => "CALLER_NOT_ALLOWED",
        404 => "MODEL_NOT_FOUND",
        429 => "QUOTA_BUDGET_EXHAUSTED",
        _ => "QUOTA_UNAVAILABLE",
    }
}

/// An error body in the wire's own shape, which AIPlex keeps.
pub fn error_body(path: &str, status: u16, message: &str) -> Value {
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
pub fn aiplex(name: &'static str) -> MockHttp {
    access_point(name, String::new())
}

/// Response headers no harness should get from AIPlex, with the values the
/// mock sends: they would point it at AIPlex or carry its state.
pub const AIPLEX_ONLY: &[(&str, &str)] = &[
    ("Set-Cookie", "aiplex_session=forged; Path=/"),
    ("Link", "<http://aiplex.test/ap/ide>; rel=\"self\""),
    ("Content-Location", "http://aiplex.test/ap/ide/v1/messages"),
    ("Location", "http://aiplex.test/ap/ide"),
    ("Via", "1.1 aiplex.test"),
    ("Server", "aiplex"),
    (
        "WWW-Authenticate",
        "Bearer resource_metadata=\"http://aiplex.test/.well-known/oauth\"",
    ),
    ("X-Aiplex-Attempts", "1"),
];

/// Response headers a harness's SDK reads, which AIPlex's answer keeps.
pub const SDK_HEADERS: &[(&str, &str)] = &[
    ("Retry-After", "3"),
    ("Retry-After-Ms", "2500"),
    ("X-Should-Retry", "true"),
    ("Request-Id", "req_provider_1"),
    ("X-Request-Id", "provider-req-1"),
    ("Anthropic-Ratelimit-Requests-Remaining", "99"),
    ("X-Ratelimit-Remaining-Tokens", "1000"),
    ("Cache-Control", "no-cache"),
];

/// An error body that echoes `Bearer` [`AIPLEX_TOKEN`], compressed, so the
/// token is not in its bytes: by API (Anthropic's shape, then OpenAI's) and
/// coding. Made with Python's `gzip.compress(mtime=0)` and `zlib.compress`.
const COMPRESSED: [(&str, &str, &str); 4] = [
    ("anthropic", "gzip", "H4sIAAAAAAAC/z3KMQqAMAxG4auEzHZw7ehFJMiPFmsraQSl9O6KoNuD91W2awd7Yqhm5e4LT/VfctiCZGESCzmNv9xQiswvufJB5TE0QBRKEvaI0ykkOssrkuu5tRueAkzHbgAAAA=="),
    ("anthropic", "deflate", "eJw9yjEKgDAMRuGrhMx2cO3oRSTIjxZrK2kEpfTuiqDbg/dVtmsHe2KoZuXuC0/1X3LYgmRhEgs5jb/cUIrML7nyQeUxNEAUShL2iNMpJDrLK5LrubUbAioluw=="),
    ("openai", "gzip", "H4sIAAAAAAAC/x3KQQqAIBBG4asMs04oaNWymwz1U5GpjApJePey3fvgPQxVrzzRwxdilA1fc/GZIlyiGaJQkiNY3EYh1iR/wpmBO+JUwr9LTnvz4tfmsR9qfQHrZ88KWgAAAA=="),
    ("openai", "deflate", "eJwdykEKgCAQRuGrDLNOKGjVspsM9VORqYwKSXj3st374D0MVa880cMXYpQNX3PxmSJcohmiUJIjWNxGIdYkf8KZgTviVMK/S0578+LX5rEfan0BX4QdBA=="),
];

/// Headers that would carry the token or point at AIPlex in their values,
/// or break their grammar, each a case the gateway drops: by name.
pub fn hostile_headers(host: &str) -> Vec<(String, String)> {
    let token = AIPLEX_TOKEN;
    vec![
        (
            "X-Request-Id".into(),
            format!("https://aiplex.example/x?t={token}"),
        ),
        (format!("x-ratelimit-{token}"), "1".into()),
        (
            "anthropic-ratelimit-requests-reset".into(),
            "https://aiplex.example/reset".into(),
        ),
        ("Request-Id".into(), "r".repeat(60_000)),
        ("Retry-After".into(), "1000000000000".into()),
        ("X-Should-Retry".into(), "maybe".into()),
        ("Retry-After-Ms".into(), host.to_owned()),
        ("x-ratelimit-limit-tokens".into(), format!("at {host}")),
    ]
}

/// The chunked body of a streamed error whose text holds `secret` split
/// across two chunks.
fn split_across_chunks(secret: &str) -> Vec<u8> {
    let half = secret.len() / 2;
    let first = format!(
        "event: error\ndata: {{\"error\": \"you sent {}",
        &secret[..half]
    );
    let second = format!("{}\"}}\n\n", &secret[half..]);
    format!(
        "{:x}\r\n{first}\r\n{:x}\r\n{second}\r\n0\r\n\r\n",
        first.len(),
        second.len()
    )
    .into_bytes()
}

/// [`aiplex`], whose `redirect-NNN` answers redirect to `redirect_to`.
/// Besides `refuse-` and `relay-`: `echo-NNN` refuses with the
/// `Authorization` it was sent in its body (`echo-stream-NNN` in a chunked
/// event stream, split across chunks; `relay-echo-NNN` without the code);
/// `headers-` answers with [`AIPLEX_ONLY`] and [`SDK_HEADERS`]; `close-`
/// closes without answering; `hang-` never answers; `esc-id`, `esc-code`,
/// `newline-id` and `newline-code` put an ESC byte or a raw newline in
/// `X-Aiplex-Request-Id` or `X-Aiplex-Error-Code`.
pub fn access_point(name: &'static str, redirect_to: String) -> MockHttp {
    let n = AtomicUsize::new(0);
    MockHttp::start(move |request| {
        let id = format!("{name}-req-{}", n.fetch_add(1, Ordering::SeqCst) + 1);
        if request.method == "GET" {
            return Response::json(200, &json!({"object": "list", "data": [{"id": "m"}]}))
                .header("X-Aiplex-Request-Id", &id);
        }
        let model = model_of(request);
        let status = |rest: &str| rest.get(..3).and_then(|s| s.parse::<u16>().ok());
        let sent = request.header("authorization").unwrap_or("").to_owned();
        if let Some(status) = model.strip_prefix("echo-stream-").and_then(status) {
            return Response::new(status, split_across_chunks(&sent))
                .header("Content-Type", "text/event-stream")
                .header("Transfer-Encoding", "chunked")
                .header("X-Aiplex-Request-Id", &id)
                .header("X-Aiplex-Error-Code", code(status));
        }
        if let Some(status) = model.strip_prefix("echo-").and_then(status) {
            let echo = format!("you sent {sent}");
            return Response::json(status, &error_body(&request.path, status, &echo))
                .header("X-Aiplex-Request-Id", &id)
                .header("X-Aiplex-Error-Code", code(status));
        }
        if let Some(status) = model.strip_prefix("relay-echo-").and_then(status) {
            let echo = format!("you sent {sent}");
            return Response::json(status, &error_body(&request.path, status, &echo))
                .header("X-Aiplex-Request-Id", &id);
        }
        // A refusal of AIPlex's own, carried by a redirect.
        if let Some(status) = model.strip_prefix("refusing-redirect-").and_then(status) {
            return Response::new(status, "")
                .header("Location", &redirect_to)
                .header("X-Aiplex-Request-Id", &id)
                .header("X-Aiplex-Error-Code", "CALLER_NOT_ALLOWED");
        }
        if let Some(status) = model.strip_prefix("redirect-").and_then(status) {
            return Response::new(status, "")
                .header("Location", &redirect_to)
                .header("X-Aiplex-Request-Id", &id);
        }
        if model.starts_with("headers-") {
            let mut answer = answer(request, &model).header("X-Aiplex-Request-Id", &id);
            for (name, value) in AIPLEX_ONLY.iter().chain(SDK_HEADERS) {
                answer = answer.header(name, value);
            }
            return answer;
        }
        // A refusal whose compressed body echoes the token.
        if let Some(coding) = model.strip_prefix("encoded-") {
            use base64::Engine as _;
            let wire = match request.path.ends_with("/v1/messages") {
                true => "anthropic",
                false => "openai",
            };
            let coding = coding.split('-').next().unwrap_or("");
            let body = COMPRESSED
                .iter()
                .find(|(w, c, _)| *w == wire && *c == coding)
                .map(|(_, _, b)| base64::engine::general_purpose::STANDARD.decode(b).unwrap())
                .unwrap_or_default();
            return Response::new(401, body)
                .header("Content-Type", "application/json")
                .header("Content-Encoding", coding)
                .header("X-Aiplex-Request-Id", &id)
                .header("X-Aiplex-Error-Code", "UNAUTHENTICATED");
        }
        // A success in a coding the gateway did not ask for.
        if model.starts_with("gzipped-ok") {
            return answer(request, &model)
                .header("Content-Encoding", "gzip")
                .header("X-Aiplex-Request-Id", &id);
        }
        // Headers whose values are a channel, then a duplicate content type.
        if model.starts_with("hostile-") {
            let host = request
                .header("host")
                .unwrap_or("")
                .split(':')
                .next()
                .unwrap_or("")
                .to_owned();
            let mut answer = answer(request, &model).header("X-Aiplex-Request-Id", &id);
            for (name, value) in hostile_headers(&host) {
                answer = answer.header(&name, &value);
            }
            return answer;
        }
        if model.starts_with("typed-twice") {
            return answer(request, &model)
                .header("Content-Type", "application/json")
                .header("X-Aiplex-Request-Id", &id);
        }
        if model.starts_with("boundary-") {
            let typed = format!("application/json; boundary={AIPLEX_TOKEN}");
            let answer = answer(request, &model);
            let mut bare = Response::new(answer.status, answer.body.clone());
            bare = bare
                .header("Content-Type", &typed)
                .header("X-Aiplex-Request-Id", &id);
            return bare;
        }
        // An error body of exactly `MAX_ERROR_BODY` (1 MiB) bytes, or one
        // more: refused by AIPlex (`exact-refuse`, `over-refuse`) or the
        // provider's, relayed (`exact-relay`).
        if let Some(rest) = model.strip_prefix("exact-").or(model.strip_prefix("over-")) {
            let size = 1024 * 1024 + usize::from(model.starts_with("over-"));
            let answer = Response::new(503, "e".repeat(size))
                .header("Content-Type", "text/plain")
                .header("X-Aiplex-Request-Id", &id);
            return match rest.starts_with("refuse") {
                true => answer.header("X-Aiplex-Error-Code", "QUOTA_UNAVAILABLE"),
                false => answer,
            };
        }
        // A refusal whose body never ends: its head, then silence.
        if model.starts_with("stall-") {
            return Response::new(429, "4\r\n{\"er\r\n")
                .header("Transfer-Encoding", "chunked")
                .header("X-Aiplex-Error-Code", "QUOTA_BUDGET_EXHAUSTED")
                .keep_alive();
        }
        if model.starts_with("close-") {
            return Response::close();
        }
        if model.starts_with("hang-") {
            return Response::hang();
        }
        match model.as_str() {
            "esc-id" => return answer(request, &model).header("X-Aiplex-Request-Id", "req\x1b1"),
            "newline-id" => return answer(request, &model).header("X-Aiplex-Request-Id", "req\n1"),
            "esc-code" => {
                return Response::json(429, &error_body(&request.path, 429, "refused"))
                    .header("X-Aiplex-Error-Code", "QUOTA\x1bX")
            }
            "newline-code" => {
                return Response::json(429, &error_body(&request.path, 429, "refused"))
                    .header("X-Aiplex-Error-Code", "QUOTA\nX")
            }
            _ => {}
        }
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
pub fn model_of(request: &Request) -> String {
    serde_json::from_slice::<Value>(&request.body)
        .ok()
        .and_then(|b| b["model"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// A provider's answer to `request`, with usage.
pub fn answer(request: &Request, model: &str) -> Response {
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

/// Every header a harness could use to speak for itself (attribution,
/// identity, client address, cookies), in other cases and twice, its own
/// credential where the gateway would not read it, and the APIs' own
/// headers.
pub fn forged(api: &str) -> Value {
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
        ["X-Goog-Iap-Jwt-Assertion", "forged-iap-assertion"],
        ["x-goog-api-key", "forged-google-key"],
        ["Cookie", "session=forged-cookie"],
        ["Forwarded", "for=forged.example"],
        ["X-Forwarded-For", "203.0.113.9"],
        ["x-real-ip", "203.0.113.10"],
        ["X-Tenant", "forged-tenant"],
        ["X-USER", "forged-user"],
        ["X-Stainless-Lang", "python"],
        // The APIs' own headers, which a control plane is sent as they are.
        [
            "anthropic-beta",
            "prompt-caching-2024-07-31,tools-2024-04-04"
        ],
        ["OpenAI-Beta", "assistants=v2"],
        ["Idempotency-Key", "idem-1"],
        credential
    ])
}

pub fn call(label: &str, api: &str, path: &str, model: &str, stream: bool) -> Value {
    json!({"label": label, "api": api, "path": path, "model": model, "stream": stream,
        "headers": forged(api)})
}

/// The AIPlex backends at `first` and `second`, a provider reached
/// directly at `direct`, and their routes.
pub fn config(first: &MockHttp, second: &MockHttp, direct: &MockHttp) -> Value {
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

pub fn use_gateway(f: &Fixture, config: Value) {
    use_gateway_quiet(f, config, None);
}

/// [`use_gateway`], giving up on a backend quiet for `quiet`. A key named
/// `BY_TEST_*` is that variable of this process; any other is a file
/// beside the repository.
pub fn use_gateway_quiet(f: &Fixture, config: Value, quiet: Option<Duration>) {
    std::fs::write(f.dir.join("aiplex"), format!("{AIPLEX_TOKEN}\n")).unwrap();
    std::fs::write(f.dir.join("anthropic"), PROVIDER_KEY).unwrap();
    let config: models::Config = serde_json::from_value(config).unwrap();
    let signer = models::Signer::local(&f.yard).unwrap();
    let dir = f.dir.clone();
    let mut gateway = models::Gateway::new(&config, signer, move |name| {
        match name.starts_with("BY_TEST_") {
            true => KeySource::Env(name.to_owned()),
            false => KeySource::File(dir.join(name)),
        }
    })
    .unwrap();
    if let Some(quiet) = quiet {
        gateway = gateway.quiet_after(quiet);
    }
    f.yard.use_models(gateway);
}

/// The value of response header `name` the harness read for a call, as
/// [`run`] returns it.
pub fn got_header<'a>(read: &'a Value, name: &str) -> Vec<&'a str> {
    read["headers"]
        .as_array()
        .map(|all| {
            all.iter()
                .filter(|h| h[0].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name)))
                .filter_map(|h| h[1].as_str())
                .collect()
        })
        .unwrap_or_default()
}

/// Run the plan as branch `name`, with `trace` as the turn's trace context:
/// the branch's events and what the harness read, by label.
pub fn run(
    f: &Fixture,
    name: &str,
    trace: Option<&str>,
    plan: &Value,
) -> (Vec<RecordedEvent>, BTreeMap<String, Value>) {
    run_with(f, name, trace, plan, |_| {})
}

/// [`run`], with `adjust` changing the task's options first.
pub fn run_with(
    f: &Fixture,
    name: &str,
    trace: Option<&str>,
    plan: &Value,
    adjust: impl FnOnce(&mut TaskOptions),
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
    let mut options = options;
    adjust(&mut options);
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

pub fn calls(events: &[RecordedEvent]) -> BTreeMap<String, ModelCall> {
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
pub fn asked<'a>(seen: &'a [Request], model: &str) -> Vec<&'a Request> {
    seen.iter().filter(|r| model_of(r) == model).collect()
}

/// Every file under `dir`, read.
pub fn read_all(dir: &Path) -> String {
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
