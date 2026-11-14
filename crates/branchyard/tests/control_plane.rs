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
mod aiplex_kit;
mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use aiplex_kit::*;
use branchyard_testkit::{Banner, MockHttp, Request};
use common::{text, Fixture};
use serde_json::{json, Value};

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
        // Nothing else the harness sent speaks for it: not who it is, not
        // where it is, not a cookie.
        for gone in [
            "tracestate",
            "x-request-id",
            "x-aiplex-other",
            "x-goog-iap-jwt-assertion",
            "x-goog-api-key",
            "cookie",
            "forwarded",
            "x-forwarded-for",
            "x-real-ip",
            "x-tenant",
            "x-user",
            "x-stainless-lang",
        ] {
            assert!(request.header_values(gone).is_empty(), "{model}: {gone}");
        }
        // The APIs' own headers go as the harness sent them.
        if request.path.ends_with("/v1/messages") {
            assert_eq!(
                request.header_values("anthropic-beta"),
                ["prompt-caching-2024-07-31,tools-2024-04-04"],
                "{model}"
            );
            assert_eq!(request.header_values("anthropic-version"), ["2023-06-01"]);
        }
        assert_eq!(
            request.header_values("openai-beta"),
            ["assistants=v2"],
            "{model}"
        );
        assert_eq!(
            request.header_values("idempotency-key"),
            ["idem-1"],
            "{model}"
        );
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
        // AIPlex's own headers are Branchyard's to record, not the
        // harness's to read.
        assert_eq!(got["error_code"], Value::Null, "{label}");
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
    assert_eq!(plain[0].header_values("X-Forwarded-For"), ["203.0.113.9"]);
    assert_eq!(plain[0].header_values("cookie"), ["session=forged-cookie"]);
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

#[test]
fn an_answer_holding_the_credential_or_pointing_at_aiplex_never_reaches_the_harness() {
    let f = Fixture::new();
    let elsewhere = MockHttp::start(|_| branchyard_testkit::Response::new(200, "elsewhere"));
    let (first, second, direct) = (
        access_point("one", format!("{}/stolen", elsewhere.url())),
        access_point("two", format!("{}/stolen", elsewhere.url())),
        aiplex("direct"),
    );
    let mut config = config(&first, &second, &direct);
    config["routes"].as_array_mut().unwrap().extend([
        json!({"model": "echo-*", "backends": ["aiplex-anthropic"]}),
        json!({"model": "gpt-echo-*", "backends": ["aiplex-openai"]}),
        json!({"model": "redirect-*", "backends": ["aiplex-anthropic"],
                "fallbacks": ["spare-anthropic"]}),
        json!({"model": "headers-*", "backends": ["aiplex-anthropic"]}),
    ]);
    use_gateway(&f, config);
    let openai_echo =
        |label: &str, model: &str| call(label, "openai", "/chat/completions", model, false);
    let plan = json!([
        call("echo 401", "anthropic", "/v1/messages", "echo-401", false),
        call("echo 403", "anthropic", "/v1/messages", "echo-403", false),
        openai_echo("echo 401 openai", "echo-401-o"),
        openai_echo("echo 403 openai", "echo-403-o"),
        call(
            "echo streamed",
            "anthropic",
            "/v1/messages",
            "echo-stream-401",
            true
        ),
        call(
            "relayed echo",
            "anthropic",
            "/v1/messages",
            "relay-echo-503",
            false
        ),
        call("headers", "anthropic", "/v1/messages", "headers-ok", false),
        call(
            "redirect",
            "anthropic",
            "/v1/messages",
            "redirect-302",
            false
        ),
    ]);
    let (events, read) = run(&f, "echo", None, &plan);
    let calls = calls(&events);
    // An answer holding the credential: Branchyard's own error, the status
    // kept, the reason on the event, the body nowhere.
    for (label, model, status) in [
        ("echo 401", "echo-401", 401),
        ("echo 403", "echo-403", 403),
        ("echo 401 openai", "echo-401-o", 401),
        ("echo 403 openai", "echo-403-o", 403),
        ("echo streamed", "echo-stream-401", 401),
        ("relayed echo", "relay-echo-503", 503),
    ] {
        let got = &read[label];
        assert_eq!(got["status"], status, "{label}: {got}");
        let body = got["body"].as_str().unwrap();
        assert!(!body.contains(AIPLEX_TOKEN), "{label}: {body}");
        assert!(body.contains("withheld"), "{label}: {body}");
        let call = &calls[model];
        let reason = call.reason.as_deref().unwrap();
        assert!(reason.contains("held the credential"), "{label}: {reason}");
        assert!(call.control_plane.is_some(), "{label}");
    }
    // The echoing calls were each sent once, so the test saw what it meant.
    let seen = [first.requests(), second.requests()].concat();
    assert_eq!(asked(&seen, "echo-stream-401").len(), 1);
    assert_eq!(asked(&seen, "relay-echo-503").len(), 2);
    // Of AIPlex's answer headers, only what an SDK reads.
    let got = &read["headers"];
    assert_eq!(got["status"], 200, "{got}");
    for (name, _) in AIPLEX_ONLY {
        assert!(
            got_header(got, name).is_empty(),
            "{name} reached the harness: {got}"
        );
    }
    for (name, value) in SDK_HEADERS {
        assert_eq!(got_header(got, name), [*value], "{name}: {got}");
    }
    assert!(got_header(got, "x-aiplex-request-id").is_empty());
    // A redirect is neither followed nor relayed: both AIPlex backends
    // redirected, so the call failed, and the target heard nothing.
    let got = &read["redirect"];
    assert_eq!(got["status"], 502, "{got}");
    assert!(got_header(got, "location").is_empty(), "{got}");
    assert_eq!(asked(&seen, "redirect-302").len(), 2);
    assert!(elsewhere.requests().is_empty(), "the redirect was followed");
    let reason = calls["redirect-302"].reason.as_deref().unwrap();
    assert!(
        reason.contains("answered with a redirect (302), not followed"),
        "{reason}"
    );
    // Neither the token nor an echo of it is in the events or the audit.
    let recorded = serde_json::to_string(&events).unwrap();
    assert!(!recorded.contains(AIPLEX_TOKEN));
    let audit = read_all(&f.root.join(".branchyard/models/audit"));
    assert!(!audit.contains(AIPLEX_TOKEN));
    first.finish();
    second.finish();
    direct.finish();
    elsewhere.finish();
}

#[test]
fn a_call_sent_to_aiplex_names_its_id_whatever_came_back() {
    let f = Fixture::new();
    let (first, second) = (aiplex("one"), aiplex("two"));
    // A response head with a byte that is not UTF-8 in AIPlex's id.
    let raw = Banner::start(
        b"HTTP/1.1 200 OK\r\nX-Aiplex-Request-Id: req\xff1\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        1,
    );
    let plane = |api: &str, url: String| json!({"api": api, "url": url, "key": "aiplex", "control_plane": "aiplex"});
    let config = json!({
        "backends": {
            "ap": plane("anthropic", format!("{}/ap/ide", first.url())),
            "spare": plane("anthropic", format!("{}/ap/ide", second.url())),
            "down": plane("anthropic", "http://127.0.0.1:1/ap/ide".into()),
            "raw": plane("anthropic", format!("http://{}/ap/ide", raw.addr()))
        },
        "routes": [
            {"model": "refused-conn-*", "backends": ["down"]},
            {"model": "hang-solo-*", "backends": ["ap"]},
            {"model": "hang-*", "backends": ["ap"], "fallbacks": ["spare"]},
            {"model": "raw-*", "backends": ["raw"]},
            {"model": "*", "backends": ["ap"]}
        ]
    });
    // The product's own clock: a backend quiet for this long has failed.
    use_gateway_quiet(&f, config, Some(Duration::from_secs(3)));
    let mut through = call(
        "pass-through",
        "anthropic",
        "/v1/messages",
        "claude-x",
        false,
    );
    through["raw_path"] = json!("/backends/ap/v1/messages");
    let plan = json!([
        call(
            "refused",
            "anthropic",
            "/v1/messages",
            "refused-conn-1",
            false
        ),
        call("closed", "anthropic", "/v1/messages", "close-1", false),
        call("timeout", "anthropic", "/v1/messages", "hang-1", false),
        call(
            "timeout alone",
            "anthropic",
            "/v1/messages",
            "hang-solo-1",
            false
        ),
        call("esc id", "anthropic", "/v1/messages", "esc-id", false),
        call(
            "newline id",
            "anthropic",
            "/v1/messages",
            "newline-id",
            false
        ),
        call("esc code", "anthropic", "/v1/messages", "esc-code", false),
        call(
            "newline code",
            "anthropic",
            "/v1/messages",
            "newline-code",
            false
        ),
        call("not utf-8", "anthropic", "/v1/messages", "raw-1", false),
        through,
    ]);
    let (events, read) = run(&f, "noanswer", None, &plan);
    let calls = calls(&events);
    let seen = [first.requests(), second.requests()].concat();
    // No answer, or one the gateway cannot read: Branchyard's own error,
    // nothing of AIPlex's kept, and the event names the id the call was
    // sent under.
    for (label, model) in [
        ("refused", "refused-conn-1"),
        ("closed", "close-1"),
        ("timeout", "hang-1"),
        ("timeout alone", "hang-solo-1"),
        ("esc id", "esc-id"),
        ("newline id", "newline-id"),
        ("esc code", "esc-code"),
        ("newline code", "newline-code"),
    ] {
        let got = &read[label];
        assert_eq!(got["status"], 502, "{label}: {got}");
        assert!(
            got["body"]
                .as_str()
                .unwrap()
                .contains("Branchyard's model gateway"),
            "{label}: {got}"
        );
        let call = &calls[model];
        let plane = call.control_plane.as_deref();
        assert!(
            plane.is_some_and(|p| !p.call_id.is_empty()),
            "{label}: {call:?}"
        );
        let plane = plane.unwrap();
        assert_eq!(
            (&plane.request_id, &plane.error_code),
            (&None, &None),
            "{label}"
        );
        // The id is the one AIPlex was sent, where it got the call.
        for request in asked(&seen, model) {
            assert_eq!(
                request.header("X-Client-Request-Id"),
                Some(plane.call_id.as_str())
            );
        }
    }
    // A byte that is not UTF-8 is legal in a header value (obs-text): the
    // codec reads it as U+FFFD, the id is outside the contract's alphabet
    // and is not kept, and the answer reaches the harness without it. (The
    // listener closes once it has written, so the gateway can also see the
    // connection reset first and answer 502; either way nothing is kept.)
    let got = &read["not utf-8"];
    assert!(got["status"] == 200 || got["status"] == 502, "{got}");
    assert!(got_header(got, "x-aiplex-request-id").is_empty(), "{got}");
    let plane = calls["raw-1"].control_plane.as_deref().unwrap();
    assert!(!plane.call_id.is_empty());
    assert_eq!((&plane.request_id, &plane.error_code), (&None, &None));
    // Quiet after the call was sent: it may have been billed, so it is not
    // sent to the second backend.
    assert_eq!(asked(&first.requests(), "hang-1").len(), 1);
    assert!(asked(&second.requests(), "hang-1").is_empty());
    // A turn number past 2^31 fits a tag (unit tested); the pass-through
    // path never reaches an AIPlex backend, by its own check.
    let got = &read["pass-through"];
    assert_eq!(got["status"], 404, "{got}");
    assert!(
        got["body"]
            .as_str()
            .unwrap()
            .contains("the pass-through path never reaches"),
        "{got}"
    );
    assert!(asked(&seen, "claude-x").is_empty());
    first.finish();
    second.finish();
    raw.finish();
}

#[test]
fn an_answer_that_could_hide_the_token_or_hold_the_harness_is_not_relayed() {
    let f = Fixture::new();
    let (first, second, direct) = (aiplex("one"), aiplex("two"), aiplex("direct"));
    // A key file holding one character.
    std::fs::write(f.dir.join("short"), "e").unwrap();
    let mut config = config(&first, &second, &direct);
    config["backends"]["short-key"] = json!({"api": "anthropic",
        "url": format!("{}/ap/ide", first.url()), "key": "short", "control_plane": "aiplex"});
    let routes = config["routes"].as_array_mut().unwrap();
    routes.insert(0, json!({"model": "*-plain", "backends": ["direct"]}));
    routes.insert(0, json!({"model": "short-*", "backends": ["short-key"]}));
    routes.push(json!({"model": "*", "backends": ["aiplex-anthropic"]}));
    use_gateway_quiet(&f, config, Some(Duration::from_secs(3)));
    let plan = json!([
        call("gzip", "anthropic", "/v1/messages", "encoded-gzip", false),
        call(
            "deflate",
            "anthropic",
            "/v1/messages",
            "encoded-deflate",
            false
        ),
        call(
            "gzip openai",
            "openai",
            "/chat/completions",
            "encoded-gzip-o",
            false
        ),
        call(
            "deflate openai",
            "openai",
            "/chat/completions",
            "encoded-deflate-o",
            false
        ),
        call(
            "gzip success",
            "anthropic",
            "/v1/messages",
            "gzipped-ok",
            false
        ),
        call(
            "gzip plain",
            "anthropic",
            "/v1/messages",
            "encoded-gzip-plain",
            false
        ),
        call("hostile", "anthropic", "/v1/messages", "hostile-ok", false),
        call(
            "typed twice",
            "anthropic",
            "/v1/messages",
            "typed-twice",
            false
        ),
        call(
            "boundary",
            "anthropic",
            "/v1/messages",
            "boundary-ok",
            false
        ),
        call(
            "stream",
            "anthropic",
            "/v1/messages",
            "claude-stream-ok",
            true
        ),
        call(
            "exact refusal",
            "anthropic",
            "/v1/messages",
            "exact-refuse",
            false
        ),
        call(
            "over refusal",
            "anthropic",
            "/v1/messages",
            "over-refuse",
            false
        ),
        call(
            "exact relay",
            "anthropic",
            "/v1/messages",
            "exact-relay",
            false
        ),
        call("stall", "anthropic", "/v1/messages", "stall-1", false),
        call("short key", "anthropic", "/v1/messages", "short-1", false),
        call("timeout", "anthropic", "/v1/messages", "hang-1", false),
    ]);
    let (events, read) = run(&f, "hidden", None, &plan);
    let calls = calls(&events);
    let seen = [first.requests(), second.requests()].concat();
    // A refusal in a coding the gateway did not ask for: withheld, whatever
    // it holds, on both wires and both codings.
    for (label, model) in [
        ("gzip", "encoded-gzip"),
        ("deflate", "encoded-deflate"),
        ("gzip openai", "encoded-gzip-o"),
        ("deflate openai", "encoded-deflate-o"),
    ] {
        let got = &read[label];
        assert_eq!(got["status"], 401, "{label}: {got}");
        assert!(
            got_header(got, "content-encoding").is_empty(),
            "{label}: {got}"
        );
        assert!(
            got["body"].as_str().unwrap().contains("withheld"),
            "{label}: {got}"
        );
        let reason = calls[model].reason.as_deref().unwrap();
        assert!(reason.contains("encoded"), "{label}: {reason}");
        assert_eq!(asked(&seen, model).len(), 1);
    }
    // A success in such a coding is not relayed either: a failure, 502.
    let got = &read["gzip success"];
    assert_eq!(got["status"], 502, "{got}");
    assert_eq!(calls["gzipped-ok"].decision, "failed");
    // A plain backend's encoded answer is relayed with its coding, as before.
    let got = &read["gzip plain"];
    assert_eq!(got["status"], 401, "{got}");
    assert_eq!(got_header(got, "content-encoding"), ["gzip"], "{got}");
    // Header values that are a channel: dropped; the answer still arrives.
    let got = &read["hostile"];
    assert_eq!(got["status"], 200, "{got}");
    let text = got["headers"].to_string();
    assert!(!text.contains(AIPLEX_TOKEN), "{text}");
    assert!(!text.contains("aiplex.example"), "{text}");
    for name in [
        "x-request-id",
        "request-id",
        "retry-after",
        "x-should-retry",
        "retry-after-ms",
        "x-ratelimit-limit-tokens",
        "anthropic-ratelimit-requests-reset",
    ] {
        assert!(got_header(got, name).is_empty(), "{name}: {got}");
    }
    assert_eq!(got_header(got, "content-type"), ["application/json"]);
    // A content type that cannot be relayed (twice, or holding the token):
    // the answer is a failure of that backend, not relayed unchecked.
    for (label, model) in [("typed twice", "typed-twice"), ("boundary", "boundary-ok")] {
        let got = &read[label];
        assert_eq!(got["status"], 502, "{label}: {got}");
        assert!(!got.to_string().contains(AIPLEX_TOKEN), "{label}");
        let reason = calls[model].reason.as_deref().unwrap();
        assert!(reason.contains("Content-Type"), "{label}: {reason}");
    }
    // An ordinary stream still arrives with its content type.
    let got = &read["stream"];
    assert_eq!(got["status"], 200, "{got}");
    assert_eq!(
        got_header(got, "content-type"),
        ["text/event-stream"],
        "{got}"
    );
    // Exactly 1 MiB is checked and relayed on both paths; one byte more is
    // withheld.
    for label in ["exact refusal", "exact relay"] {
        let got = &read[label];
        assert_eq!(got["status"], 503, "{label}");
        assert_eq!(got["body"].as_str().unwrap().len(), 1024 * 1024, "{label}");
    }
    let got = &read["over refusal"];
    assert_eq!(got["status"], 503);
    assert!(got["body"].as_str().unwrap().contains("too large"), "{got}");
    // A refusal whose body stops coming: withheld once the backend has been
    // quiet for the quiet period, within the total bound.
    let got = &read["stall"];
    assert_eq!(got["status"], 429, "{got}");
    let reason = calls["stall-1"].reason.as_deref().unwrap();
    assert!(
        reason.contains("withheld") && reason.contains("timed out"),
        "{reason}"
    );
    // A one-character key is never used, and never shown.
    let got = &read["short key"];
    assert_eq!(got["status"], 502, "{got}");
    let reason = calls["short-1"].reason.as_deref().unwrap();
    assert!(
        reason.contains("short-key: its key is shorter than 16"),
        "{reason}"
    );
    assert!(asked(&seen, "short-1").is_empty());
    // A timeout says so, how long, and which backend.
    let reason = calls["hang-1"].reason.as_deref().unwrap();
    assert!(
        reason.contains("aiplex-anthropic: timed out: no answer for 3s"),
        "{reason}"
    );
    first.finish();
    second.finish();
    direct.finish();
}

#[test]
fn a_harness_that_goes_away_during_a_refusal_leaves_it_a_refusal() {
    let f = Fixture::new();
    let (first, second, direct) = (aiplex("one"), aiplex("two"), aiplex("direct"));
    let mut config = config(&first, &second, &direct);
    config["routes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"model": "exact-*", "backends": ["aiplex-anthropic"],
            "fallbacks": ["spare-anthropic"]}));
    use_gateway(&f, config);
    let early = |label: &str, model: &str| {
        let mut call = call(label, "anthropic", "/v1/messages", model, false);
        call["close_early"] = json!(true);
        call
    };
    // A small refusal, and one of 1 MiB, which the gateway is still writing
    // when the harness has gone.
    let plan = json!([
        early("small", "refuse-429"),
        early("large", "exact-refuse"),
        call("stayed", "anthropic", "/v1/messages", "refuse-403", false),
    ]);
    let (events, read) = run(&f, "gone", None, &plan);
    let calls = calls(&events);
    let seen = [first.requests(), second.requests()].concat();
    let rows = f.yard.model_usage(0).unwrap();
    for (label, model, status, code) in [
        ("small", "refuse-429", 429, "QUOTA_BUDGET_EXHAUSTED"),
        ("large", "exact-refuse", 503, "QUOTA_UNAVAILABLE"),
        ("stayed", "refuse-403", 403, "CALLER_NOT_ALLOWED"),
    ] {
        assert_eq!(read[label]["status"], status, "{label}");
        let call = &calls[model];
        // Still AIPlex's refusal: its decision, code and id, never `failed`.
        assert_eq!(call.decision, "refused", "{label}: {call:?}");
        assert_eq!(call.status, status, "{label}");
        let plane = call.control_plane.as_deref().unwrap();
        assert_eq!(plane.error_code.as_deref(), Some(code), "{label}");
        assert!(plane.request_id.is_some(), "{label}");
        let reason = call.reason.as_deref().unwrap();
        assert!(
            reason.starts_with(&format!("refused by aiplex ({code})")),
            "{label}: {reason}"
        );
        // Sent once: neither retried nor moved because the harness left.
        assert_eq!(asked(&seen, model).len(), 1, "{label}");
        // Stored as a call that stayed is: one row, its id, its status, no
        // cost (a refusal is not metered).
        let row: Vec<_> = rows.iter().filter(|r| r.model == model).collect();
        assert_eq!(row.len(), 1, "{label}");
        assert_eq!(row[0].id, plane.call_id, "{label}");
        assert_eq!(row[0].status, status, "{label}");
        assert_eq!(row[0].cost_usd, None, "{label}");
        println!("{label}: {reason}");
    }
    first.finish();
    second.finish();
    direct.finish();
}
