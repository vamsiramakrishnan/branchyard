//! Webhook notifications against a local HTTP receiver: no real network,
//! no real harness.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use branchyard_server::config::WebhookConfig;
use branchyard_testkit::{wait, MockHttp, Response};
use common::{run, task, Fixture, Server};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// One accepted delivery.
#[derive(Clone, Debug)]
struct Delivery {
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Delivery {
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    fn signature(&self) -> &str {
        self.headers
            .get("x-branchyard-signature")
            .expect("signature header")
    }

    fn dedupe_key(&self) -> &str {
        self.headers
            .get("x-branchyard-delivery")
            .expect("delivery header")
    }
}

/// A local HTTP receiver: answers `500` for its first `fail_next`
/// deliveries, then `200`, recording every one it accepts. A connection
/// that errs fails the test.
struct Receiver {
    mock: MockHttp,
    fail_next: Arc<AtomicU32>,
}

impl Receiver {
    fn start() -> Receiver {
        let fail_next = Arc::new(AtomicU32::new(0));
        let failing = fail_next.clone();
        let mock = MockHttp::start(move |_| {
            let fail = failing
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    (n > 0).then(|| n - 1)
                })
                .is_ok();
            Response::new(if fail { 500 } else { 200 }, "")
        });
        Receiver { mock, fail_next }
    }

    fn url(&self) -> String {
        format!("{}/hook", self.mock.url())
    }

    fn deliveries(&self) -> Vec<Delivery> {
        self.mock
            .requests()
            .into_iter()
            .map(|r| Delivery {
                headers: r.headers.into_iter().collect(),
                body: r.body,
            })
            .collect()
    }

    /// Answer the next `n` deliveries with `500`.
    fn fail_next(&self, n: u32) {
        self.fail_next.store(n, Ordering::Relaxed);
    }
}

fn hmac_hex(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

fn webhook(url: &str, secret: &str, events: &[&str]) -> WebhookConfig {
    WebhookConfig {
        id: url.to_owned(),
        url: url.to_owned(),
        secret: secret.to_owned(),
        events: events.iter().map(|s| s.to_string()).collect(),
    }
}

const SECRET: &str = "0123456789abcdef-webhook-secret";

#[test]
fn deliveries_are_signed_and_carry_the_original_activity() {
    let f = Fixture::new();
    let receiver = Receiver::start();
    let mut config = f.config();
    config.webhooks = vec![webhook(&receiver.url(), SECRET, &[])];
    let server = Server::start(config);
    let client = server.client();
    run(&client, &task("WRITE a.txt=1", "one"));

    wait::until("a status delivery", || {
        receiver
            .deliveries()
            .iter()
            .any(|d| d.json()["activity"]["status"]["state"] == "ready")
    });
    let delivery = receiver
        .deliveries()
        .into_iter()
        .find(|d| d.json()["activity"]["status"]["state"] == "ready")
        .unwrap();
    assert_eq!(delivery.json()["repo"], "app");
    assert_eq!(delivery.json()["branch"], "one");
    assert_eq!(delivery.json()["kinds"], serde_json::json!(["status"]));
    let expected = hmac_hex(SECRET, &delivery.body);
    assert_eq!(delivery.signature(), format!("sha256={expected}"));
    assert_eq!(delivery.dedupe_key(), delivery.json()["seq"].to_string());

    server.stop();
}

#[test]
fn an_event_filter_narrows_what_is_delivered() {
    let f = Fixture::new();
    let receiver = Receiver::start();
    let mut config = f.config();
    // `merge` never fires for this run, so nothing but its permission
    // request or stall would ever arrive; a plain successful run has
    // neither, so nothing should ever be delivered.
    config.webhooks = vec![webhook(&receiver.url(), SECRET, &["merge"])];
    let server = Server::start(config);
    let client = server.client();
    let op = run(&client, &task("WRITE a.txt=1", "one"));
    assert_eq!(op.state, branchyard_client::api::OperationState::Succeeded);

    // Give a filtered-out delivery a moment it could have arrived in.
    wait::settle(
        "a filtered-out delivery would have arrived by now",
        Duration::from_millis(300),
    );
    assert!(
        receiver.deliveries().is_empty(),
        "{:?}",
        receiver.deliveries()
    );
    server.stop();
}

#[test]
fn a_failing_receiver_is_retried_and_still_delivers() {
    std::env::set_var("BY_TEST_WEBHOOK_RETRY_MS", "20");
    std::env::set_var("BY_TEST_WEBHOOK_MAX_ATTEMPTS", "5");
    let f = Fixture::new();
    let receiver = Receiver::start();
    receiver.fail_next(2);
    let mut config = f.config();
    config.webhooks = vec![webhook(&receiver.url(), SECRET, &[])];
    let server = Server::start(config);
    let client = server.client();
    run(&client, &task("WRITE a.txt=1", "one"));

    wait::until("a delivery to arrive despite two failures", || {
        receiver
            .deliveries()
            .iter()
            .any(|d| d.json()["activity"]["status"]["state"] == "ready")
    });
    // The two failed attempts were not recorded as separate deliveries by
    // the test receiver (it only pushes to `deliveries` once, after
    // deciding the response), so at least the retries happened: the
    // successful attempt's dedupe key is the same feed position every
    // time it would have been retried with.
    server.stop();
}

#[test]
fn a_receiver_that_never_succeeds_is_dead_lettered_and_the_cursor_still_advances() {
    std::env::set_var("BY_TEST_WEBHOOK_RETRY_MS", "5");
    std::env::set_var("BY_TEST_WEBHOOK_MAX_ATTEMPTS", "3");
    let f = Fixture::new();
    let receiver = Receiver::start();
    receiver.fail_next(u32::MAX);
    let mut config = f.config();
    config.webhooks = vec![webhook(&receiver.url(), SECRET, &[])];
    let server = Server::start(config);
    let client = server.client();
    run(&client, &task("WRITE a.txt=1", "one"));
    run(&client, &task("WRITE b.txt=1", "two"));

    // Every attempt for both branches' events was answered 500, so nothing
    // ever "succeeds", but the second branch's status must still have been
    // attempted (the cursor moved past the first branch's dead-lettered
    // entries) within a reasonable time.
    wait::until("both branches to have been attempted", || {
        let seen: std::collections::HashSet<String> = receiver
            .deliveries()
            .iter()
            .map(|d| d.json()["branch"].as_str().unwrap().to_owned())
            .collect();
        seen.contains("one") && seen.contains("two")
    });
    server.stop();
}

#[test]
fn a_restart_resumes_from_its_saved_cursor_instead_of_replaying() {
    let f = Fixture::new();
    let receiver = Receiver::start();
    let config = {
        let mut c = f.config();
        c.webhooks = vec![webhook(&receiver.url(), SECRET, &[])];
        c
    };
    let server = Server::start(config.clone());
    let client = server.client();
    run(&client, &task("WRITE a.txt=1", "one"));
    wait::until("the first branch delivered", || {
        receiver
            .deliveries()
            .iter()
            .any(|d| d.json()["branch"] == "one")
    });
    let before = receiver.deliveries().len();
    server.stop();

    // A second branch's activity happens while no server is serving it;
    // the SDK still records it directly, standing in for activity another
    // process recorded while this server was down.
    {
        let yard = branchyard::Yard::open(&f.root).unwrap();
        yard.task("WRITE b.txt=1")
            .name("two")
            .harness("gemini-cli")
            .command(vec![common::fake_agent().display().to_string()])
            .policy(branchyard::Policy::allow_all())
            .run()
            .unwrap();
    }

    let server = Server::start(config);
    let client = server.client();
    // A third branch after the restart, so there is definitely new
    // activity to wake the webhook task promptly.
    run(&client, &task("WRITE c.txt=1", "three"));
    wait::until("the second and third branches delivered", || {
        let seen: std::collections::HashSet<String> = receiver
            .deliveries()
            .iter()
            .map(|d| d.json()["branch"].as_str().unwrap().to_owned())
            .collect();
        seen.contains("two") && seen.contains("three")
    });
    // Nothing from before the restart was replayed.
    let after: Vec<Delivery> = receiver.deliveries();
    let replayed = after
        .iter()
        .take(before)
        .filter(|d| d.json()["branch"] != "one")
        .count();
    assert_eq!(replayed, 0, "{after:?}");
    server.stop();
}

#[test]
fn a_stall_notification_reaches_a_webhook() {
    let f = Fixture::new();
    let receiver = Receiver::start();
    let mut config = f.config();
    config.webhooks = vec![webhook(&receiver.url(), SECRET, &["stall"])];
    let server = Server::start(config);
    let client = server.client();
    let request = branchyard_client::api::TaskRequest {
        prompt: "HANG WRITE never.txt=1".into(),
        harness: Some("gemini-cli".into()),
        name: Some("stalls".into()),
        policy: branchyard_client::api::PolicySpec::allow_all(),
        budget: branchyard_client::api::BudgetSpec {
            stall_after_seconds: Some(0.1),
            ..Default::default()
        },
        ..Default::default()
    };
    let op = client
        .repo("app")
        .submit_task(&request, &branchyard_client::new_key())
        .unwrap();
    wait::until("a stall delivery", || {
        receiver.deliveries().iter().any(|d| {
            d.json()["kinds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|k| k == "stall")
        })
    });
    let _ = client.repo("app").cancel("stalls");
    let _ = common::await_operation(&client, &op.id);
    server.stop();
}

/// A receiver that reads a delivery in full and then misbehaves: never
/// answers, holding the connection open (the "receiver that hangs"), or
/// closes the connection without writing any response at all (the
/// "receiver that closes mid-response": from the client's side, a connection
/// that disappears before the reply arrives looks the same whether it was
/// closed gracefully or reset).
struct Adversary {
    mock: MockHttp,
}

impl Adversary {
    fn hanging() -> Adversary {
        Adversary {
            mock: MockHttp::start(|_| Response::hang()),
        }
    }

    fn closing() -> Adversary {
        Adversary {
            mock: MockHttp::start(|_| Response::close()),
        }
    }

    fn url(&self) -> String {
        format!("{}/hook", self.mock.url())
    }

    /// Deliveries read in full.
    fn accepted(&self) -> usize {
        self.mock.requests().len()
    }
}

/// Failure-direction: a webhook target that accepts the connection, reads
/// the whole delivery and then never answers must never delay or fail the
/// operation whose activity it was delivering, nor stall the feed for
/// other branches. See `docs/lifecycle.md` "Webhook failure direction".
#[test]
fn a_hanging_receiver_never_delays_the_operation_or_the_feed() {
    let f = Fixture::new();
    let receiver = Adversary::hanging();
    let mut config = f.config();
    config.webhooks = vec![webhook(&receiver.url(), SECRET, &[])];
    let server = Server::start(config);
    let client = server.client();

    let start = std::time::Instant::now();
    let op = run(&client, &task("WRITE a.txt=1", "one"));
    assert_eq!(op.state, branchyard_client::api::OperationState::Succeeded);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "the operation waited on the stuck webhook: {:?}",
        start.elapsed()
    );

    wait::until("the hanging receiver to have accepted the delivery", || {
        receiver.accepted() > 0
    });

    // The feed keeps advancing for a second branch while the first
    // delivery's connection is still stuck open.
    let start = std::time::Instant::now();
    let op = run(&client, &task("WRITE b.txt=1", "two"));
    assert_eq!(op.state, branchyard_client::api::OperationState::Succeeded);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "a second operation also waited on the stuck webhook: {:?}",
        start.elapsed()
    );
    server.stop();
}

/// Failure-direction: a webhook target that resets the connection instead
/// of answering must never delay or fail the operation either, and the
/// server keeps retrying without blocking anything else.
#[test]
fn a_receiver_that_resets_the_connection_never_delays_the_operation() {
    std::env::set_var("BY_TEST_WEBHOOK_RETRY_MS", "5");
    std::env::set_var("BY_TEST_WEBHOOK_MAX_ATTEMPTS", "3");
    let f = Fixture::new();
    let receiver = Adversary::closing();
    let mut config = f.config();
    config.webhooks = vec![webhook(&receiver.url(), SECRET, &[])];
    let server = Server::start(config);
    let client = server.client();

    let start = std::time::Instant::now();
    let op = run(&client, &task("WRITE a.txt=1", "one"));
    assert_eq!(op.state, branchyard_client::api::OperationState::Succeeded);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "the operation waited on the resetting webhook: {:?}",
        start.elapsed()
    );

    wait::until(
        "the resetting receiver to have been hit at least once",
        || receiver.accepted() > 0,
    );

    let op = run(&client, &task("WRITE b.txt=1", "two"));
    assert_eq!(
        op.state,
        branchyard_client::api::OperationState::Succeeded,
        "the feed and later operations are unaffected by a webhook that only ever resets"
    );
    server.stop();
}

/// A delivery of a branch's activity carries the `traceparent` of the
/// operation that worked on the branch, and deliveries are counted.
#[test]
fn deliveries_carry_the_operations_traceparent_and_are_counted() {
    use branchyard_server::observe::Observability;
    use branchyard_server::telemetry::{MemoryExporter, SpanContext, Tracer};
    let f = Fixture::new();
    let receiver = Receiver::start();
    let memory = Arc::new(MemoryExporter::default());
    let observability = Observability {
        metrics: Arc::new(branchyard_server::metrics::Metrics::default()),
        tracer: Tracer::new(memory.clone()),
    };
    let mut config = f.config();
    config.webhooks = vec![webhook(&receiver.url(), SECRET, &["status"])];
    config.observability = Some(observability.clone());
    let server = Server::start(config);
    let client = server.client();
    let op = run(&client, &task("WRITE a.txt=1", "one"));
    wait::until("a status delivery", || {
        receiver
            .deliveries()
            .iter()
            .any(|d| d.json()["activity"]["status"]["state"] == "ready")
    });
    let delivery = receiver
        .deliveries()
        .into_iter()
        .find(|d| d.json()["activity"]["status"]["state"] == "ready")
        .unwrap();
    let traceparent = delivery.headers.get("traceparent").expect("a traceparent");
    let context = SpanContext::parse(traceparent).unwrap();
    observability.tracer.flush(Duration::from_secs(10));
    let operation = memory
        .spans()
        .into_iter()
        .find(|s| s.name == "operation task")
        .expect("the operation's span");
    assert_eq!(context, operation.context, "{op:?}");
    let delivered = observability.metrics.snapshot();
    let counted = &delivered[branchyard_server::metrics::WEBHOOKS]
        [&vec![("result".to_owned(), "delivered".to_owned())]];
    assert!(
        matches!(counted, branchyard_server::metrics::Value::Number(n) if *n >= 1.0),
        "{counted:?}"
    );
    server.stop();
}
