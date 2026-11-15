//! Beside a control plane, a plain backend is sent only what a route names
//! for it (`docs/model-gateway.md`, "What a plain backend beside an AIPlex
//! one is sent"). With an AIPlex backend configured: one list of routes for
//! both kinds of backend; a model no route names, and a request that names
//! none, is AIPlex's or nobody's; the model list is answered as before; an
//! AIPlex route reads a model's name in any spelling; the pass-through path
//! is closed. Without one, nothing changes.
//!
//! Through the engine, as `tests/control_plane.rs`: a fake harness (a
//! Python script the fake ACP agent runs) calls through its turn's gateway,
//! and `MockHttp` servers play an AIPlex access point and a provider
//! reached directly. Hermetic: nothing leaves this host.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod aiplex_kit;
mod common;

use aiplex_kit::*;
use branchyard_testkit::MockHttp;
use common::{text, Fixture};
use serde_json::{json, Value};

/// Which API has which kind of backend, in a configuration that has both.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mixed {
    /// AIPlex and a plain backend on each API.
    EachApi,
    /// AIPlex on OpenAI's API only; the plain backend is on Anthropic's.
    PlainAnthropic,
    /// AIPlex on Anthropic's API only; the plain backend is on OpenAI's.
    PlainOpenai,
}

const MIXED: [Mixed; 3] = [Mixed::EachApi, Mixed::PlainAnthropic, Mixed::PlainOpenai];

impl Mixed {
    /// Whether `api` has an AIPlex backend.
    fn aiplex(self, api: &str) -> bool {
        match self {
            Mixed::EachApi => true,
            Mixed::PlainAnthropic => api == "openai",
            Mixed::PlainOpenai => api == "anthropic",
        }
    }

    /// Whether `api` has a plain backend.
    fn plain(self, api: &str) -> bool {
        match self {
            Mixed::EachApi => true,
            Mixed::PlainAnthropic => api == "anthropic",
            Mixed::PlainOpenai => api == "openai",
        }
    }

    /// The configuration. Every backend of the two APIs has a route:
    /// `claude-*` and `gpt-*` to AIPlex (`aiplex-anthropic`,
    /// `aiplex-openai`), `direct-*` and `odirect-*` to the plain ones
    /// (`direct`, `direct-openai`). The plain OpenAI backend has a
    /// compatibility layer, so its provider is asked the unversioned paths;
    /// a generic backend (`gen`) stands beside them.
    fn config(self, access: &MockHttp, plain: &MockHttp) -> Value {
        let at = format!("{}/ap/ide", access.url());
        let mut backends =
            json!({"gen": {"api": "generic", "url": plain.url(), "key": "anthropic"}});
        let mut routes = Vec::new();
        for (api, pattern) in [("anthropic", "claude-*"), ("openai", "gpt-*")] {
            if self.aiplex(api) {
                backends[format!("aiplex-{api}")] = json!({"api": api, "url": at,
                    "key": "aiplex", "control_plane": "aiplex"});
                routes.push(json!({"model": pattern,
                    "backends": [format!("aiplex-{api}")]}));
            }
        }
        if self.plain("anthropic") {
            backends["direct"] =
                json!({"api": "anthropic", "url": plain.url(), "key": "anthropic"});
            routes.push(json!({"model": "direct-*", "backends": ["direct"]}));
        }
        if self.plain("openai") {
            backends["direct-openai"] = json!({"api": "openai", "url": plain.url(),
                "key": "anthropic", "compat": "versioned"});
            routes.push(json!({"model": "odirect-*", "backends": ["direct-openai"]}));
        }
        json!({"backends": backends, "routes": routes})
    }
}

/// A body the gateway reads, naming `model`.
fn naming(model: &str) -> Vec<u8> {
    json!({"model": model, "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]})
    .to_string()
    .into_bytes()
}

/// A body that names `model` to a reader more lenient than the gateway's
/// parser, which does not read it: a byte that is not UTF-8 in its text.
fn hiding(model: &str) -> Vec<u8> {
    let mut body = format!(
        "{{\"model\":\"{model}\",\"max_tokens\":16,\"messages\":[{{\"role\":\"user\",\"content\":\"hi "
    )
    .into_bytes();
    body.push(0xff);
    body.extend_from_slice(b"\"}]}");
    body
}

/// A request with `body` as it is, under `content_type`: `method` to `path`
/// after `api`'s base URL (`OPENAI_BASE_URL` already ends in `/v1`).
fn sent(
    label: &str,
    api: &str,
    method: &str,
    path: &str,
    body: &[u8],
    content_type: &str,
) -> Value {
    json!({"label": label, "api": api, "path": path, "method": method, "model": "",
        "body_bytes": body, "content_type": content_type, "headers": []})
}

/// `call`, sent to `path` at the gateway's root instead.
fn at_root(mut call: Value, path: &str) -> Value {
    call["raw_path"] = json!(path);
    call
}

/// One request a harness could send to carry a model past the routes.
struct Way {
    /// The plan's entry.
    call: Value,
    /// The body it carries.
    body: Vec<u8>,
    /// What a gateway that has a control plane answers it with.
    refused: u16,
    /// What a gateway without one asks its provider: the method and path.
    /// `None` for the pass-through path to a backend of a model API, which
    /// such a gateway has never sent on (the request speaks no API, and
    /// nothing translates it): it fails there, `502`, and nothing is sent.
    forwarded: Option<(String, String)>,
    /// What a gateway without one answers it with: `200` from the provider
    /// for what it forwards, else its own refusal.
    unchanged: u16,
}

/// Other spellings of `POST /v1/messages` and of
/// `POST /v1/messages/count_tokens`, which a provider may read as the route.
const SPELLINGS: [&str; 14] = [
    "/v1/messages/",
    "//v1/messages",
    "/v1//messages",
    "/v1/./messages",
    "/v1/x/../messages",
    "/v1/%6dessages",
    "/v1%2Fmessages",
    "/v1/messages%3Fx",
    "/v1/messages#frag",
    "/v1/messages;x=1",
    "/V1/MESSAGES",
    "/v1\\messages",
    "/v1/messages%20",
    "/v1/messages/count_tokens/",
];

/// What reached a plain backend beside an AIPlex one under version 1 of the
/// integration, for a model whose route names AIPlex: another spelling of a
/// route's path, another method, the unversioned paths, another endpoint of
/// the provider, a batch, a multipart body, the pass-through path.
///
/// Most are sent twice. Once with a body the gateway's parser does not read,
/// naming an AIPlex-routed model: version 1 read no model from it, so no
/// route took it, and it went to the plain backend of its API. And once
/// with a body the gateway reads, naming a model with a plain route:
/// version 1 sent a plain backend whatever path the harness asked for.
fn past_the_routes() -> Vec<Way> {
    let mut ways = Vec::new();
    let mut way = |label: String,
                   api: &str,
                   method: &str,
                   path: &str,
                   body: Vec<u8>,
                   content_type: &str,
                   refused: u16| {
        // What the gateway reads after the API's prefix, and so what a
        // plain backend with no compatibility layer is asked.
        let forwarded = match api {
            "openai" => format!("/v1{path}"),
            _ => path.to_owned(),
        };
        ways.push(Way {
            call: sent(&label, api, method, path, &body, content_type),
            body,
            refused,
            forwarded: Some((method.to_owned(), forwarded)),
            unchanged: 200,
        });
    };
    let mut twice = |api: &str, method: &str, path: &str| {
        let (governed, plain) = match api {
            "openai" => ("gpt-x", "odirect-ok"),
            _ => ("claude-x", "direct-ok"),
        };
        way(
            format!("{method} {api} {path}, a body the gateway does not read"),
            api,
            method,
            path,
            hiding(governed),
            "text/plain",
            403,
        );
        way(
            format!("{method} {api} {path}, a model with a plain route"),
            api,
            method,
            path,
            naming(plain),
            "application/json",
            403,
        );
    };
    for path in SPELLINGS {
        twice("anthropic", "POST", path);
    }
    for method in ["post", "PUT", "PATCH", "DELETE", "QUERY", "GET"] {
        twice("anthropic", method, "/v1/messages");
    }
    // Another endpoint of the provider.
    twice("openai", "POST", "/embeddings");
    // A batch: its top-level model has a plain route, and the model its
    // requests would run is AIPlex's.
    let batch = json!({"model": "direct-ok", "requests": [{"custom_id": "a",
        "params": {"model": "claude-x", "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]}}]});
    way(
        "a batch".to_owned(),
        "anthropic",
        "POST",
        "/v1/messages/batches",
        batch.to_string().into_bytes(),
        "application/json",
        403,
    );
    // A multipart body names its model in a form field: on another
    // endpoint, and on a route, where the body rule refuses it.
    let form = b"--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ngpt-x\r\n--b--\r\n";
    for (label, path, refused) in [
        ("a multipart body", "/audio/transcriptions", 403),
        ("a multipart body on a route", "/chat/completions", 400),
    ] {
        way(
            label.to_owned(),
            "openai",
            "POST",
            path,
            form.to_vec(),
            "multipart/form-data; boundary=b",
            refused,
        );
    }
    // The paths a harness is given end in `/v1` for OpenAI's API; the
    // unversioned ones are other spellings, and a backend with a
    // compatibility layer is asked them as it is the versioned ones.
    for path in ["/chat/completions", "/responses"] {
        for (what, body, content_type) in [
            (
                "a body the gateway does not read",
                hiding("gpt-x"),
                "text/plain",
            ),
            (
                "a model with a plain route",
                naming("odirect-ok"),
                "application/json",
            ),
        ] {
            let label = format!("POST /openai{path} (unversioned), {what}");
            ways.push(Way {
                call: at_root(
                    sent(&label, "openai", "POST", "", &body, content_type),
                    &format!("/openai{path}"),
                ),
                body,
                refused: 403,
                forwarded: Some(("POST".to_owned(), path.to_owned())),
                unchanged: 200,
            });
        }
    }
    // The pass-through path: to the generic backend, and to each plain one
    // by its name, for an AIPlex-routed model and for the backend's own.
    for (name, model) in [
        ("gen", "claude-x"),
        ("direct", "claude-x"),
        ("direct", "direct-ok"),
        ("direct-openai", "gpt-x"),
    ] {
        let label = format!("the pass-through path to {name} for {model}");
        let body = naming(model);
        ways.push(Way {
            call: at_root(
                sent(&label, "anthropic", "POST", "", &body, "application/json"),
                &format!("/backends/{name}/v1/messages"),
            ),
            body,
            refused: 404,
            forwarded: (name == "gen").then(|| ("POST".to_owned(), "/v1/messages".to_owned())),
            unchanged: if name == "gen" { 200 } else { 502 },
        });
    }
    // A body that says it is JSON and is not: on a path that is not a route
    // the route list answers first (the order is the integration's), and on
    // a route the body rule does. A gateway without a control plane refuses
    // both before the token, as it always has.
    let broken = br#"{"model":"claude-x","#.to_vec();
    for (label, path, refused) in [
        ("broken JSON off the routes", "/v1/messages/", 403),
        ("broken JSON on a route", "/v1/messages", 400),
    ] {
        ways.push(Way {
            call: sent(
                label,
                "anthropic",
                "POST",
                path,
                &broken,
                "application/json",
            ),
            body: broken.clone(),
            refused,
            forwarded: None,
            unchanged: 400,
        });
    }
    ways
}

#[test]
fn what_carried_a_model_past_the_routes_reaches_no_backend_beside_a_control_plane() {
    for kind in MIXED {
        let f = Fixture::new();
        let (access, plain) = (provider(), provider());
        use_gateway(&f, kind.config(&access, &plain));
        let ways = past_the_routes();
        let plan: Vec<Value> = ways.iter().map(|way| way.call.clone()).collect();
        let (events, read) = run(&f, "past", None, &Value::Array(plan));
        // Each is refused by the gateway itself, with an event that has
        // the reason and no backend.
        let recorded = all_calls(&events);
        assert_eq!(recorded.len(), ways.len(), "{kind:?}: {}", text(&events));
        for (way, event) in ways.iter().zip(&recorded) {
            let label = way.call["label"].as_str().unwrap();
            let got = &read[label];
            assert_eq!(got["status"], way.refused, "{kind:?}, {label}: {got}");
            let said = match way.refused {
                403 => "is not a route the gateway serves while a control plane is configured",
                404 => "Branchyard's model gateway: no backend ",
                _ => "the gateway routes such a request only when it read the model",
            };
            let body = got["body"].as_str().unwrap();
            assert!(body.contains(said), "{kind:?}, {label}: {body}");
            assert_eq!(event.status, way.refused, "{kind:?}, {label}");
            assert_eq!(
                (&event.backend, &event.control_plane),
                (&None, &None),
                "{kind:?}, {label}"
            );
        }
        // Neither stand-in heard any of them.
        assert!(
            plain.requests().is_empty(),
            "{kind:?}: the plain backend was sent {:?}",
            plain.requests()
        );
        assert!(
            access.requests().is_empty(),
            "{kind:?}: {:?}",
            access.requests()
        );
        access.finish();
        plain.finish();
    }
}

#[test]
fn a_gateway_without_a_control_plane_forwards_each_of_them_as_it_always_did() {
    let f = Fixture::new();
    let plain = provider();
    // The plain backends of the mixed configurations and their routes, and
    // no AIPlex backend. (No compatibility layer here, so the provider is
    // asked each path as the gateway read it.)
    use_gateway(
        &f,
        json!({
            "backends": {
                "direct": {"api": "anthropic", "url": plain.url(), "key": "anthropic"},
                "direct-openai": {"api": "openai", "url": plain.url(), "key": "anthropic"},
                "gen": {"api": "generic", "url": plain.url(), "key": "anthropic"}
            },
            "routes": [
                {"model": "direct-*", "backends": ["direct"]},
                {"model": "odirect-*", "backends": ["direct-openai"]}
            ]
        }),
    );
    let ways = past_the_routes();
    let plan: Vec<Value> = ways.iter().map(|way| way.call.clone()).collect();
    let (events, read) = run(&f, "open", None, &Value::Array(plan));
    // Every one the gateway forwards reached the provider, with the
    // method, the path and the bytes the harness sent; the pass-through
    // path to a model API's backend failed in the gateway, and a body
    // that says it is JSON and is not was refused, as they always were,
    // and reached nothing.
    let heard = plain.requests();
    let forwarded: Vec<&Way> = ways.iter().filter(|w| w.forwarded.is_some()).collect();
    assert_eq!(heard.len(), forwarded.len(), "{}", text(&events));
    for way in &ways {
        let label = way.call["label"].as_str().unwrap();
        assert_eq!(
            read[label]["status"], way.unchanged,
            "{label}: {}",
            read[label]
        );
    }
    for (way, request) in forwarded.iter().zip(&heard) {
        let label = way.call["label"].as_str().unwrap();
        let (method, path) = way.forwarded.as_ref().unwrap();
        assert_eq!(
            (request.method.as_str(), request.path.as_str()),
            (method.as_str(), path.as_str()),
            "{label}"
        );
        assert_eq!(request.body, way.body, "{label}");
    }
    plain.finish();
}

/// The four routes whose body names the model, as the harness addresses
/// them (`OPENAI_BASE_URL` already ends in `/v1`).
const ROUTES: [(&str, &str); 4] = [
    ("anthropic", "/v1/messages"),
    ("anthropic", "/v1/messages/count_tokens"),
    ("openai", "/chat/completions"),
    ("openai", "/responses"),
];

#[test]
fn a_request_no_route_names_is_aiplexs_or_nobodys_and_never_a_plain_backends() {
    for kind in MIXED {
        let f = Fixture::new();
        let (access, plain) = (aiplex("one"), provider());
        use_gateway(&f, kind.config(&access, &plain));
        // On each route: a model no route names, then a body that names
        // none.
        let nameless = json!({"max_tokens": 16, "input": "hi",
            "messages": [{"role": "user", "content": "hi"}]})
        .to_string();
        let mut plan = Vec::new();
        let mut asked_for: Vec<(String, &str, &str, Option<String>)> = Vec::new();
        for (n, (api, path)) in ROUTES.into_iter().enumerate() {
            let model = format!("llama-{n}");
            let label = format!("a model no route names, {api} {path}");
            plan.push(sent(
                &label,
                api,
                "POST",
                path,
                &naming(&model),
                "application/json",
            ));
            asked_for.push((label, api, path, Some(model)));
            let label = format!("no model, {api} {path}");
            plan.push(sent(
                &label,
                api,
                "POST",
                path,
                nameless.as_bytes(),
                "application/json",
            ));
            asked_for.push((label, api, path, None));
        }
        let (events, read) = run(&f, "nobody", None, &Value::Array(plan));
        let recorded = all_calls(&events);
        assert_eq!(
            recorded.len(),
            asked_for.len(),
            "{kind:?}: {}",
            text(&events)
        );
        let mut answered = Vec::new();
        for ((label, api, path, model), event) in asked_for.iter().zip(&recorded) {
            let got = &read[label.as_str()];
            if kind.aiplex(api) {
                // The API has an AIPlex backend: it answers.
                assert_eq!(got["status"], 200, "{kind:?}, {label}: {got}");
                assert_eq!(
                    event.backend.as_deref(),
                    Some(format!("aiplex-{api}").as_str()),
                    "{kind:?}, {label}"
                );
                assert!(event.control_plane.is_some(), "{kind:?}, {label}");
                let version = if *api == "openai" { "/v1" } else { "" };
                answered.push(format!("POST /ap/ide{version}{path}"));
                continue;
            }
            // It has none, and the other API has: sent nowhere. The event
            // says what to add; the harness is told no backend's name.
            assert_eq!(got["status"], 404, "{kind:?}, {label}: {got}");
            let (why, told) = match model {
                Some(model) => (
                    format!(
                        "no route that can take a request in the {api} API names {model}, and \
                         beside a control plane a model no route names is an AIPlex backend's, \
                         of which this API has none, so the request is sent nowhere: add a \
                         route for {model}; a last route with model = \"*\" that names a plain \
                         backend sends it everything no earlier route names"
                    ),
                    format!(
                        "no route that can take this request names {model}, and it is not sent \
                         to a backend no route names for it"
                    ),
                ),
                None => (
                    format!(
                        "this request names no model, so no route can name it, and beside a \
                         control plane such a request is an AIPlex backend's; the {api} API \
                         has none, so it is sent nowhere: name a model that a route names for \
                         the {api} API, or give the {api} API an AIPlex backend"
                    ),
                    "this request names no model, and it is not sent to a backend no route \
                     names for it"
                        .to_owned(),
                ),
            };
            let body = got["body"].as_str().unwrap();
            assert!(body.contains(&told), "{kind:?}, {label}: {body}");
            assert!(
                !body.contains("direct") && !body.contains("aiplex"),
                "{kind:?}, {label}: {body}"
            );
            assert_eq!(
                (event.decision.as_str(), event.status),
                ("failed", 404),
                "{kind:?}, {label}"
            );
            assert_eq!(
                event.reason.as_deref(),
                Some(why.as_str()),
                "{kind:?}, {label}"
            );
            assert_eq!(
                (&event.backend, &event.control_plane),
                (&None, &None),
                "{kind:?}, {label}"
            );
        }
        // AIPlex heard what its own APIs were asked, at each route's path,
        // and the plain backend heard nothing.
        let heard: Vec<String> = access
            .requests()
            .iter()
            .map(|r| format!("{} {}", r.method, r.path))
            .collect();
        assert_eq!(heard, answered, "{kind:?}");
        assert!(
            plain.requests().is_empty(),
            "{kind:?}: the plain backend was sent {:?}",
            plain.requests()
        );
        access.finish();
        plain.finish();
    }
}

#[test]
fn a_model_name_that_is_only_white_space_names_no_model_beside_a_control_plane() {
    // The name of a real model is never white space: beside a control
    // plane, one that is empty once trimmed is no model, as `""` is, so a
    // last route `*` to a plain backend does not take it.
    let names = ["", " ", "\t", "\u{a0}", "\n"];
    let plan = |f: &Fixture, name: &str| {
        let plan: Vec<Value> = names
            .iter()
            .enumerate()
            .flat_map(|(n, model)| {
                [
                    call(
                        &format!("anthropic {n}"),
                        "anthropic",
                        "/v1/messages",
                        model,
                        false,
                    ),
                    call(
                        &format!("openai {n}"),
                        "openai",
                        "/chat/completions",
                        model,
                        false,
                    ),
                ]
            })
            .collect();
        let count = plan.len();
        let (_, read) = run(f, name, None, &Value::Array(plan));
        for api in ["anthropic", "openai"] {
            for n in 0..names.len() {
                let got = &read[format!("{api} {n}").as_str()];
                assert_eq!(got["status"], 200, "{api} {n}: {got}");
            }
        }
        count
    };
    let plain_routes = json!([
        {"model": "*", "backends": ["direct"]},
        {"model": "*", "backends": ["direct-openai"]}
    ]);
    let plain_backends = |plain: &MockHttp| {
        json!({
            "direct": {"api": "anthropic", "url": plain.url(), "key": "anthropic"},
            "direct-openai": {"api": "openai", "url": plain.url(), "key": "anthropic"}
        })
    };
    // Beside AIPlex on each API, with a last route `*` to a plain backend
    // of each: every one of them goes to AIPlex, as written.
    let f = Fixture::new();
    let (access, plain) = (aiplex("one"), aiplex("direct"));
    let mut backends = plain_backends(&plain);
    let mut routes = vec![
        json!({"model": "claude-*", "backends": ["aiplex-anthropic"]}),
        json!({"model": "gpt-*", "backends": ["aiplex-openai"]}),
    ];
    routes.extend(plain_routes.as_array().unwrap().iter().cloned());
    for api in ["anthropic", "openai"] {
        backends[format!("aiplex-{api}")] = json!({"api": api,
            "url": format!("{}/ap/ide", access.url()), "key": "aiplex",
            "control_plane": "aiplex"});
    }
    use_gateway(&f, json!({"backends": backends, "routes": routes}));
    let count = plan(&f, "blank");
    let heard: Vec<String> = access.requests().iter().map(model_of).collect();
    let expected: Vec<String> = names
        .iter()
        .flat_map(|name| [name.to_string(), name.to_string()])
        .collect();
    assert_eq!(heard, expected);
    assert_eq!(heard.len(), count);
    assert!(
        plain.requests().is_empty(),
        "the plain backend was sent {:?}",
        plain.requests()
    );
    access.finish();
    plain.finish();
    // A plain-only gateway: a name that is white space is a name, as it
    // always was, and `*` takes it; `""` names no model and goes to the
    // plain backends of its API, as it always did.
    let f = Fixture::new();
    let plain = aiplex("direct");
    use_gateway(
        &f,
        json!({"backends": plain_backends(&plain), "routes": plain_routes}),
    );
    plan(&f, "blank-plain");
    let heard: Vec<String> = plain.requests().iter().map(model_of).collect();
    assert_eq!(heard, expected);
    plain.finish();
}

#[test]
fn a_model_a_route_names_for_the_other_api_only_is_said_so_when_refused() {
    // AIPlex on Anthropic's API only, and a plain route `direct-*` to a
    // plain Anthropic backend: an OpenAI request for `direct-x` has no
    // backend to go to (OpenAI's API has a plain backend, for its own
    // route, and no AIPlex one). Its reason says what is true: no route
    // that can take it names it, and a route names it for the other API.
    let f = Fixture::new();
    let (access, plain) = (aiplex("one"), aiplex("direct"));
    use_gateway(
        &f,
        json!({
            "backends": {
                "aiplex-anthropic": {"api": "anthropic",
                    "url": format!("{}/ap/ide", access.url()), "key": "aiplex",
                    "control_plane": "aiplex"},
                "direct": {"api": "anthropic", "url": plain.url(), "key": "anthropic"},
                "direct-openai": {"api": "openai", "url": plain.url(), "key": "anthropic"}
            },
            "routes": [
                {"model": "claude-*", "backends": ["aiplex-anthropic"]},
                {"model": "direct-*", "backends": ["direct"]},
                {"model": "odirect-*", "backends": ["direct-openai"]}
            ]
        }),
    );
    let plan = json!([
        call(
            "other api",
            "openai",
            "/chat/completions",
            "direct-x",
            false
        ),
        call("own api", "anthropic", "/v1/messages", "direct-y", false),
    ]);
    let (events, read) = run(&f, "elsewhere", None, &plan);
    let got = &read["other api"];
    assert_eq!(got["status"], 404, "{got}");
    let body = got["body"].as_str().unwrap();
    assert!(
        body.contains(
            "no route that can take this request names direct-x, and it is not sent to a \
             backend no route names for it"
        ),
        "{body}"
    );
    assert!(
        !body.contains("direct-*") && !body.contains("anthropic"),
        "{body}"
    );
    assert_eq!(
        calls(&events)["direct-x"].reason.as_deref(),
        Some(
            "no route that can take a request in the openai API names direct-x (the route \
             \"direct-*\" names it for a request in the anthropic API), and beside a control \
             plane a model no route names is an AIPlex backend's, of which this API has none, \
             so the request is sent nowhere: add a route for direct-x; a last route with model \
             = \"*\" that names a plain backend sends it everything no earlier route names"
        )
    );
    assert_eq!(read["own api"]["status"], 200, "{}", read["own api"]);
    let heard: Vec<String> = plain.requests().iter().map(model_of).collect();
    assert_eq!(heard, ["direct-y"]);
    assert!(access.requests().is_empty());
    access.finish();
    plain.finish();
}

#[test]
fn an_aiplex_route_reads_a_models_name_in_any_spelling_and_a_plain_route_as_written() {
    let f = Fixture::new();
    let (access, plain) = (aiplex("one"), aiplex("direct"));
    // `*` to the plain backend, written after the AIPlex route: the one
    // route that gives a plain backend everything no earlier route names.
    use_gateway(
        &f,
        json!({
            "backends": {
                "aiplex-anthropic": {"api": "anthropic",
                    "url": format!("{}/ap/ide", access.url()), "key": "aiplex",
                    "control_plane": "aiplex"},
                "direct": {"api": "anthropic", "url": plain.url(), "key": "anthropic"}
            },
            "routes": [
                {"model": "claude-*", "backends": ["aiplex-anthropic"]},
                {"model": "*", "backends": ["direct"]}
            ]
        }),
    );
    // The name as the route writes it, in another letter case, and with
    // white space after and before it.
    let spellings = ["claude-x", "Claude-X", "claude-x ", " CLAUDE-x"];
    // What `*` takes: a name no AIPlex route matches in any spelling. The
    // second is the limit the guide states, pinned so that it is a
    // decision: a vendor prefix a provider may read as the same model is
    // another name to the gateway, which cannot know a provider's aliases.
    let others = ["llama-3", "anthropic/claude-x"];
    let plan: Vec<Value> = spellings
        .iter()
        .chain(&others)
        .enumerate()
        .map(|(n, model)| {
            call(
                &format!("call {n}"),
                "anthropic",
                "/v1/messages",
                model,
                false,
            )
        })
        .collect();
    let count = plan.len();
    let (events, read) = run(&f, "spelled", None, &Value::Array(plan));
    for n in 0..count {
        let got = &read[format!("call {n}").as_str()];
        assert_eq!(got["status"], 200, "call {n}: {got}");
    }
    // AIPlex was sent each spelling as it was written, and the plain
    // backend none of them.
    let heard: Vec<String> = access.requests().iter().map(model_of).collect();
    assert_eq!(heard, spellings);
    let heard: Vec<String> = plain.requests().iter().map(model_of).collect();
    assert_eq!(heard, others);
    let calls = calls(&events);
    for model in spellings {
        let event = &calls[model];
        assert_eq!(
            event.backend.as_deref(),
            Some("aiplex-anthropic"),
            "{model:?}"
        );
        assert!(event.control_plane.is_some(), "{model:?}");
    }
    for model in others {
        let event = &calls[model];
        assert_eq!(event.backend.as_deref(), Some("direct"), "{model:?}");
        assert_eq!(event.control_plane, None, "{model:?}");
    }
    access.finish();
    plain.finish();
}

#[test]
fn a_model_with_a_plain_route_is_served_by_its_backend_on_every_listed_route() {
    let f = Fixture::new();
    let (access, plain) = (aiplex("one"), aiplex("direct"));
    use_gateway(&f, Mixed::EachApi.config(&access, &plain));
    // The label, the API, the path, the model, whether it is streamed, and
    // what the provider is asked (the plain OpenAI backend's compatibility
    // layer stands for `/v1`).
    let served = [
        (
            "messages",
            "anthropic",
            "/v1/messages",
            "direct-ok",
            false,
            "POST /v1/messages",
        ),
        (
            "messages streamed",
            "anthropic",
            "/v1/messages",
            "direct-ok-stream",
            true,
            "POST /v1/messages",
        ),
        (
            "count",
            "anthropic",
            "/v1/messages/count_tokens",
            "direct-ok-count",
            false,
            "POST /v1/messages/count_tokens",
        ),
        (
            "chat",
            "openai",
            "/chat/completions",
            "odirect-ok",
            false,
            "POST /chat/completions",
        ),
        (
            "chat streamed",
            "openai",
            "/chat/completions",
            "odirect-ok-stream",
            true,
            "POST /chat/completions",
        ),
        (
            "responses",
            "openai",
            "/responses",
            "odirect-ok-responses",
            false,
            "POST /responses",
        ),
        (
            "responses streamed",
            "openai",
            "/responses",
            "odirect-ok-responses-stream",
            true,
            "POST /responses",
        ),
    ];
    let plan: Vec<Value> = served
        .iter()
        .map(|(label, api, path, model, stream, _)| call(label, api, path, model, *stream))
        .collect();
    let (events, read) = run(&f, "served", None, &Value::Array(plan));
    let calls = calls(&events);
    let heard = plain.requests();
    assert_eq!(heard.len(), served.len(), "{heard:?}");
    for ((label, api, _, model, _, asked), request) in served.iter().zip(&heard) {
        assert_eq!(read[*label]["status"], 200, "{label}: {}", read[*label]);
        assert_eq!(
            format!("{} {}", request.method, request.path),
            *asked,
            "{label}"
        );
        assert_eq!(model_of(request), *model, "{label}");
        // With the provider's key, as its API takes it, and never AIPlex's.
        let (header, key) = match *api {
            "anthropic" => ("x-api-key", PROVIDER_KEY.to_owned()),
            _ => ("authorization", format!("Bearer {PROVIDER_KEY}")),
        };
        assert_eq!(request.header_values(header), [key.as_str()], "{label}");
        let event = &calls[*model];
        let backend = match *api {
            "anthropic" => "direct",
            _ => "direct-openai",
        };
        assert_eq!(event.decision, "allowed", "{label}: {event:?}");
        assert_eq!(event.backend.as_deref(), Some(backend), "{label}");
        assert_eq!(event.control_plane, None, "{label}");
    }
    assert!(access.requests().is_empty(), "{:?}", access.requests());
    access.finish();
    plain.finish();
}

#[test]
fn the_model_list_is_answered_as_before_and_carries_no_body_to_either_kind() {
    for kind in MIXED {
        let f = Fixture::new();
        let (access, plain) = (aiplex("one"), provider());
        use_gateway(&f, kind.config(&access, &plain));
        // OPENAI_BASE_URL already ends in /v1; ANTHROPIC_BASE_URL does not.
        let list = |label: &str, api: &str, body: Option<&[u8]>| {
            let path = match api {
                "openai" => "/models?limit=5",
                _ => "/v1/models?limit=5",
            };
            let mut call = json!({"label": label, "api": api, "path": path, "model": "",
                "method": "GET", "headers": forged(api)});
            if let Some(body) = body {
                call["body_bytes"] = json!(body);
            }
            call
        };
        let plan = json!([
            list("anthropic", "anthropic", None),
            list("openai", "openai", None),
            // With a body the list could name a model: a model with a
            // plain route, and an AIPlex-routed one.
            list(
                "anthropic with a body",
                "anthropic",
                Some(&naming("direct-ok"))
            ),
            list("openai with a body", "openai", Some(&naming("gpt-x"))),
        ]);
        let (events, read) = run(&f, "listed", None, &plan);
        // Answered by the plain backend of its API when there is one, and
        // otherwise by its AIPlex backend: the one request a plain backend
        // is sent without a route naming it.
        let mut to_plain = Vec::new();
        let mut to_aiplex = Vec::new();
        for api in ["anthropic", "openai"] {
            assert_eq!(read[api]["status"], 200, "{kind:?}, {api}: {}", read[api]);
            match (kind.plain(api), api) {
                (true, "anthropic") => to_plain.push("GET /v1/models?limit=5"),
                // The compatibility layer stands for `/v1`.
                (true, _) => to_plain.push("GET /models?limit=5"),
                (false, _) => to_aiplex.push("GET /ap/ide/v1/models?limit=5"),
            }
        }
        let heard = |mock: &MockHttp| -> Vec<String> {
            mock.requests()
                .iter()
                .map(|r| format!("{} {}", r.method, r.path))
                .collect()
        };
        assert_eq!(heard(&plain), to_plain, "{kind:?}");
        assert_eq!(heard(&access), to_aiplex, "{kind:?}");
        for request in plain.requests().iter().chain(&access.requests()) {
            assert!(request.body.is_empty(), "{kind:?}: {request:?}");
        }
        // A GET with a body is refused, whoever would have got it.
        for label in ["anthropic with a body", "openai with a body"] {
            let got = &read[label];
            assert_eq!(got["status"], 400, "{kind:?}, {label}: {got}");
            assert!(
                got["body"].as_str().unwrap().contains(
                    "a GET with a body is not sent on while a control plane is configured"
                ),
                "{kind:?}, {label}: {got}"
            );
        }
        let refused: Vec<_> = all_calls(&events)
            .into_iter()
            .filter(|call| call.status == 400)
            .collect();
        assert_eq!(refused.len(), 2, "{kind:?}: {}", text(&events));
        for call in &refused {
            assert_eq!((&call.backend, &call.control_plane), (&None, &None));
        }
        access.finish();
        plain.finish();
    }
}

#[test]
fn an_aiplex_backend_no_route_names_is_accepted_beside_a_plain_one_and_takes_what_no_route_names() {
    let f = Fixture::new();
    let (access, plain) = (aiplex("one"), provider());
    // No route at all: version 1 refused this configuration, since the
    // plain backend took every call and the AIPlex one would never be
    // used. Now the AIPlex backend takes every call, and the plain one is
    // sent the model list and nothing else.
    use_gateway(
        &f,
        json!({"backends": {
            "ap": {"api": "anthropic", "url": format!("{}/ap/ide", access.url()),
                "key": "aiplex", "control_plane": "aiplex"},
            "direct": {"api": "anthropic", "url": plain.url(), "key": "anthropic"}
        }}),
    );
    let plan = json!([
        call("a model", "anthropic", "/v1/messages", "claude-x", false),
        call("another", "anthropic", "/v1/messages", "llama-3", true),
        sent(
            "no model",
            "anthropic",
            "POST",
            "/v1/messages",
            br#"{"max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#,
            "application/json"
        ),
        {"label": "the model list", "api": "anthropic", "path": "/v1/models", "model": "",
            "method": "GET", "headers": []},
    ]);
    let (events, read) = run(&f, "unrouted", None, &plan);
    for label in ["a model", "another", "no model", "the model list"] {
        assert_eq!(read[label]["status"], 200, "{label}: {}", read[label]);
    }
    let heard: Vec<String> = access
        .requests()
        .iter()
        .map(|r| format!("{} {} {}", r.method, r.path, model_of(r)))
        .collect();
    assert_eq!(
        heard,
        [
            "POST /ap/ide/v1/messages claude-x",
            "POST /ap/ide/v1/messages llama-3",
            "POST /ap/ide/v1/messages ",
        ]
    );
    let heard: Vec<String> = plain
        .requests()
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect();
    assert_eq!(heard, ["GET /v1/models"]);
    let calls = calls(&events);
    for model in ["claude-x", "llama-3"] {
        assert_eq!(calls[model].backend.as_deref(), Some("ap"), "{model}");
        assert!(calls[model].control_plane.is_some(), "{model}");
    }
    access.finish();
    plain.finish();
}

#[test]
fn nothing_off_the_listed_routes_reaches_a_backend_through_a_translating_route() {
    let f = Fixture::new();
    let (access, plain) = (aiplex("one"), aiplex("direct"));
    // Only OpenAI's API has backends, one of each kind, and each has a
    // route that translates a Claude request for it.
    use_gateway(
        &f,
        json!({
            "backends": {
                "aiplex-openai": {"api": "openai", "url": format!("{}/ap/ide", access.url()),
                    "key": "aiplex", "control_plane": "aiplex"},
                "direct-openai": {"api": "openai", "url": plain.url(), "key": "anthropic"}
            },
            "routes": [
                {"model": "claude-haiku-*", "backends": ["aiplex-openai"],
                    "backend_model": "gpt-5-mini"},
                {"model": "claude-direct-*", "backends": ["direct-openai"],
                    "backend_model": "gpt-5-nano"}
            ]
        }),
    );
    let mut plan = Vec::new();
    let mut refused = Vec::new();
    for (kind, model) in [("aiplex", "claude-haiku-4-5"), ("plain", "claude-direct-1")] {
        // The route that is translated, for each kind of backend.
        plan.push(call(
            &format!("{kind} translated"),
            "anthropic",
            "/v1/messages",
            model,
            false,
        ));
        // Paths and a method that are not routes: refused before the route
        // is looked at, so nothing is translated for either kind.
        for (method, path) in [
            ("POST", "/v1/messages/"),
            ("POST", "/v1/messages/batches"),
            ("POST", "/v1/complete"),
            ("POST", "/v1/chat/completions"),
            ("PUT", "/v1/messages"),
        ] {
            let label = format!("{kind} {method} {path}");
            plan.push(sent(
                &label,
                "anthropic",
                method,
                path,
                &naming(model),
                "application/json",
            ));
            refused.push(label);
        }
    }
    let (events, read) = run(&f, "bridged", None, &Value::Array(plan));
    for label in &refused {
        let got = &read[label.as_str()];
        assert_eq!(got["status"], 403, "{label}: {got}");
        assert!(
            got["body"].as_str().unwrap().contains(
                "is not a route the gateway serves while a control plane is configured (POST \
                 /v1/messages, POST /v1/messages/count_tokens, GET /v1/models)"
            ),
            "{label}: {got}"
        );
    }
    let denied = all_calls(&events)
        .into_iter()
        .filter(|call| call.decision == "denied")
        .count();
    assert_eq!(denied, refused.len(), "{}", text(&events));
    // Each backend heard its one translated call, as a chat completion for
    // its route's model.
    for label in ["aiplex translated", "plain translated"] {
        assert_eq!(read[label]["status"], 200, "{label}: {}", read[label]);
    }
    let heard = |mock: &MockHttp| -> Vec<String> {
        mock.requests()
            .iter()
            .map(|r| format!("{} {} {}", r.method, r.path, model_of(r)))
            .collect()
    };
    assert_eq!(
        heard(&access),
        ["POST /ap/ide/v1/chat/completions gpt-5-mini"]
    );
    assert_eq!(heard(&plain), ["POST /v1/chat/completions gpt-5-nano"]);
    access.finish();
    plain.finish();
}
