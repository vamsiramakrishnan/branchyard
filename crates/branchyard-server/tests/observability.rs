//! Priority, metrics and traces over real HTTP with the fake ACP agent:
//! see `docs/server.md#scheduling` and `docs/observability.md`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use branchyard_client::api::{OperationState, PolicySpec, SpawnRequest, TaskRequest};
use branchyard_client::new_key;
use branchyard_server::config::{MetricsConfig, Principal, TenantPolicy, Token};
use branchyard_server::metrics::Metrics;
use branchyard_server::observe::Observability;
use branchyard_server::telemetry::{Attr, MemoryExporter, SpanContext, Tracer};
use common::{get, post, raw, run, task, wait, Fixture, Server, TOKEN};

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body:?}"))
}

const METRICS_TOKEN: &str = "metrics-token-0123456789";
const READER_TOKEN: &str = "reader-token-0123456789";

/// A request's priority is checked (-10 to 10), capped at its tenant's
/// `max_priority`, reported on the operation, and inherited by a spawned
/// child that names none.
#[test]
fn priority_is_checked_capped_reported_and_inherited() {
    let f = Fixture::new();
    let mut config = f.config();
    config.allow_delegation = true;
    config.by_path = Some("/bin/true".into());
    config.tenants.insert(
        "default".into(),
        TenantPolicy {
            max_priority: Some(6),
            ..TenantPolicy::default()
        },
    );
    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");

    let refused = repo
        .submit_task(
            &TaskRequest {
                priority: Some(11),
                ..task("say hi", "too-high")
            },
            &new_key(),
        )
        .unwrap_err();
    assert!(refused.to_string().contains("priority 11"), "{refused}");

    let capped = run(
        &client,
        &TaskRequest {
            priority: Some(9),
            delegation: Some(branchyard::Envelope::depth(2)),
            ..task("say hi", "parent")
        },
    );
    assert_eq!(capped.state, OperationState::Succeeded, "{capped:?}");
    assert_eq!(capped.priority, 6, "capped at the tenant's max_priority");
    let low = run(
        &client,
        &TaskRequest {
            priority: Some(-4),
            ..task("say hi", "low")
        },
    );
    assert_eq!(low.priority, -4);
    // The wire form leaves out priority 0.
    let plain = run(&client, &task("say hi", "plain"));
    assert_eq!(plain.priority, 0);

    let child = |name: &str, priority: Option<i32>| SpawnRequest {
        prompt: "say hi".into(),
        name: Some(name.into()),
        policy: PolicySpec::allow_all(),
        priority,
        ..SpawnRequest::default()
    };
    let inherited = repo
        .spawn("parent", &child("inherits", None), &new_key())
        .unwrap();
    assert_eq!(inherited.priority, 6, "a child inherits its parent's");
    assert_eq!(
        wait(&client, &inherited.id).state,
        OperationState::Succeeded
    );
    let own = repo
        .spawn("parent", &child("own", Some(-2)), &new_key())
        .unwrap();
    assert_eq!(own.priority, -2, "unless it names its own");
    assert_eq!(wait(&client, &own.id).state, OperationState::Succeeded);
    server.stop();
}

fn metrics_config(f: &Fixture, listen: bool) -> branchyard_server::Config {
    let mut config = f.config();
    config.tokens.push(Token {
        name: "reader".into(),
        secret: READER_TOKEN.into(),
    });
    config.principals.insert(
        "reader".into(),
        Principal {
            name: "reader".into(),
            tenant: "default".into(),
            scopes: ["read", "run"].into_iter().map(String::from).collect(),
            repos: None,
        },
    );
    config.metrics = Some(MetricsConfig {
        listen: listen.then(|| "127.0.0.1:0".parse().unwrap()),
        token_sha256: Some(branchyard_server::config::sha256_hex(
            METRICS_TOKEN.as_bytes(),
        )),
    });
    config
}

/// The value of the sample `name{labels}` in `text`, labels in the order
/// written.
fn sample(text: &str, name_and_labels: &str) -> Option<f64> {
    text.lines()
        .find_map(|line| line.strip_prefix(&format!("{name_and_labels} ")))
        .map(|v| v.parse().unwrap())
}

/// `/metrics` is off by default; when on, it needs the `admin` scope or the
/// metrics token, which reads nothing else; it reports the queue, claims,
/// finished operations and turns, also on a listener of its own.
#[test]
fn metrics_need_an_operator_or_the_metrics_token_and_count_the_work() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let (status, _, _) = raw(server.addr, &get("/metrics", Some(TOKEN)));
    assert_eq!(status, 404, "off by default");
    server.stop();

    let f = Fixture::new();
    let server = Server::start(metrics_config(&f, true));
    let client = server.client();
    let op = run(&client, &task("WRITE a.txt=1", "one"));
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");

    let (status, _, _) = raw(server.addr, &get("/metrics", None));
    assert_eq!(status, 401);
    let (status, _, body) = raw(server.addr, &get("/metrics", Some(READER_TOKEN)));
    assert_eq!(status, 403, "{body}");
    assert_eq!(json(&body)["error"]["detail"]["scope"], "admin");
    // The metrics token reads metrics, and nothing else.
    let (status, _, _) = raw(server.addr, &get("/v1/repos", Some(METRICS_TOKEN)));
    assert_eq!(status, 401);
    let (status, head, by_token) = raw(server.addr, &get("/metrics", Some(METRICS_TOKEN)));
    assert_eq!(status, 200, "{by_token}");
    assert!(
        head.to_lowercase().contains("text/plain; version=0.0.4"),
        "{head}"
    );
    let (status, _, by_admin) = raw(server.addr, &get("/metrics", Some(TOKEN)));
    assert_eq!(status, 200);
    for text in [&by_token, &by_admin] {
        assert_eq!(
            sample(
                text,
                r#"branchyard_operations_admitted_total{kind="task",tenant="default"}"#
            ),
            Some(1.0),
            "{text}"
        );
        assert_eq!(
            sample(
                text,
                r#"branchyard_operations_finished_total{kind="task",state="succeeded"}"#
            ),
            Some(1.0)
        );
        assert_eq!(
            sample(
                text,
                r#"branchyard_claims_total{tenant="default",priority="0"}"#
            ),
            Some(1.0)
        );
        assert_eq!(
            sample(
                text,
                r#"branchyard_turns_started_total{harness="gemini-cli"}"#
            ),
            Some(1.0)
        );
        assert_eq!(
            sample(
                text,
                r#"branchyard_turns_ended_total{harness="gemini-cli",outcome="completed"}"#
            ),
            Some(1.0)
        );
        assert_eq!(
            sample(
                text,
                r#"branchyard_turn_duration_seconds_count{harness="gemini-cli"}"#
            ),
            Some(1.0)
        );
        assert_eq!(
            sample(text, r#"branchyard_operations{state="queued"}"#),
            Some(0.0)
        );
        assert_eq!(sample(text, "branchyard_workers_live"), Some(1.0));
    }

    // The separate listener: /metrics only, with the metrics token.
    let url = server.running.as_ref().unwrap().metrics_url().unwrap();
    let addr: std::net::SocketAddr = url
        .trim_start_matches("http://")
        .trim_end_matches("/metrics")
        .parse()
        .unwrap();
    let (status, _, _) = raw(addr, &get("/metrics", None));
    assert_eq!(status, 401);
    let (status, _, _) = raw(addr, &get("/metrics", Some(TOKEN)));
    assert_eq!(status, 401, "the API's tokens are not the metrics token");
    let (status, _, text) = raw(addr, &get("/metrics", Some(METRICS_TOKEN)));
    assert_eq!(status, 200);
    assert!(text.contains("# TYPE branchyard_claims_total counter"));
    let (status, _, _) = raw(addr, &get("/v1/repos", Some(METRICS_TOKEN)));
    assert_eq!(status, 404);
    server.stop();
}

/// An operation is traced from its admission (continuing the request's
/// `traceparent`) through its claim and run to each turn, and the turn's
/// harness gets the operation's span as `TRACEPARENT`.
#[test]
fn an_operation_is_traced_from_admission_to_its_turns() {
    let f = Fixture::new();
    let memory = Arc::new(MemoryExporter::default());
    let tracer = Tracer::new(memory.clone());
    let mut config = f.config();
    config.observability = Some(Observability {
        metrics: Arc::new(Metrics::default()),
        tracer: tracer.clone(),
    });
    let server = Server::start(config);
    let client = server.client();
    let incoming = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let body = serde_json::to_string(&TaskRequest {
        priority: Some(3),
        ..task("ENV TRACEPARENT", "traced")
    })
    .unwrap();
    let (status, _, answer) = raw(
        server.addr,
        &post(
            "/v1/repos/app/tasks",
            Some(TOKEN),
            &format!("traceparent: {incoming}\r\n"),
            &body,
        ),
    );
    assert_eq!(status, 202, "{answer}");
    let id = json(&answer)["id"].as_str().unwrap().to_owned();
    assert_eq!(wait(&client, &id).state, OperationState::Succeeded);
    tracer.flush(Duration::from_secs(10));
    let spans = memory.spans();
    let named = |name: &str| {
        spans
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no {name} span in {spans:#?}"))
    };
    let parent = SpanContext::parse(incoming).unwrap();
    let admission = named("admission");
    assert_eq!(admission.context.trace_id, parent.trace_id);
    assert_eq!(admission.parent, Some(parent.span_id));
    assert_eq!(admission.attribute("by.priority"), Some(&Attr::Int(3)));
    let claim = named("claim");
    assert_eq!(claim.parent, Some(admission.context.span_id));
    let operation = named("operation task");
    assert_eq!(operation.parent, Some(admission.context.span_id));
    assert_eq!(operation.context.trace_id, parent.trace_id);
    let turn = named("turn");
    assert_eq!(turn.parent, Some(operation.context.span_id));
    assert_eq!(
        turn.attribute("by.outcome"),
        Some(&Attr::Str("completed".into()))
    );
    assert!(turn.start_ns <= turn.end_ns);
    // The harness saw the operation's span as its parent.
    let said: String = client
        .repo("app")
        .events("traced", 0)
        .unwrap()
        .events
        .iter()
        .filter_map(|e| match &e.activity {
            branchyard::Activity::Harness(branchyard::Event::MessageDelta { text, .. }) => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect();
    assert!(
        said.contains(&format!("TRACEPARENT={}", operation.context.traceparent())),
        "{said}"
    );
    // A malformed traceparent starts a trace of its own.
    let (status, _, answer) = raw(
        server.addr,
        &post(
            "/v1/repos/app/tasks",
            Some(TOKEN),
            "traceparent: nonsense\r\n",
            &serde_json::to_string(&task("say hi", "fresh")).unwrap(),
        ),
    );
    assert_eq!(status, 202, "{answer}");
    let id = json(&answer)["id"].as_str().unwrap().to_owned();
    assert_eq!(wait(&client, &id).state, OperationState::Succeeded);
    tracer.flush(Duration::from_secs(10));
    let fresh = memory
        .spans()
        .into_iter()
        .find(|s| {
            s.name == "admission" && s.attribute("by.operation") == Some(&Attr::Str(id.clone()))
        })
        .unwrap();
    assert_eq!(fresh.parent, None);
    assert_ne!(fresh.context.trace_id, parent.trace_id);
    server.stop();
}
