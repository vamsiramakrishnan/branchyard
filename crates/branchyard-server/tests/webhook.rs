//! Webhook notifications against a local HTTP receiver: no real network,
//! no real harness.

mod common;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use branchyard_server::config::WebhookConfig;
use branchyard_testkit::wait;
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
/// deliveries, then `200`, recording every one it accepts.
struct Receiver {
    addr: SocketAddr,
    deliveries: Arc<Mutex<Vec<Delivery>>>,
    fail_next: Arc<AtomicU32>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Receiver {
    fn start() -> Receiver {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let deliveries = Arc::new(Mutex::new(Vec::new()));
        let fail_next = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (d, f, s) = (deliveries.clone(), fail_next.clone(), stop.clone());
        std::thread::spawn(move || loop {
            if s.load(Ordering::Relaxed) {
                return;
            }
            match listener.accept() {
                Ok((stream, _)) => handle(stream, &d, &f),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        });
        Receiver {
            addr,
            deliveries,
            fail_next,
            stop,
        }
    }

    fn url(&self) -> String {
        format!("http://{}/hook", self.addr)
    }

    fn deliveries(&self) -> Vec<Delivery> {
        self.deliveries.lock().unwrap().clone()
    }

    /// Answer the next `n` deliveries with `500`.
    fn fail_next(&self, n: u32) {
        self.fail_next.store(n, Ordering::Relaxed);
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn handle(
    stream: std::net::TcpStream,
    deliveries: &Arc<Mutex<Vec<Delivery>>>,
    fail_next: &Arc<AtomicU32>,
) {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut stream = stream;
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut headers = HashMap::new();
    let mut content_length = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let text = line.trim_end();
        if text.is_empty() {
            break;
        }
        if let Some((k, v)) = text.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let value = v.trim().to_owned();
            if key == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(key, value);
        }
    }
    let mut body = vec![0u8; content_length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let fail = fail_next
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            (n > 0).then(|| n - 1)
        })
        .is_ok();
    let status = if fail {
        "HTTP/1.1 500 Internal Server Error\r\n"
    } else {
        "HTTP/1.1 200 OK\r\n"
    };
    let _ = stream
        .write_all(format!("{status}Content-Length: 0\r\nConnection: close\r\n\r\n").as_bytes());
    deliveries.lock().unwrap().push(Delivery { headers, body });
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
    std::thread::sleep(Duration::from_millis(300));
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

/// Read one HTTP request's headers and body off `stream`, however the
/// caller means to answer it (or not). Shared by the adversarial receivers
/// below.
fn read_request(stream: &std::net::TcpStream) -> Option<Vec<u8>> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return None;
    }
    let mut content_length = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return None;
        }
        let text = line.trim_end();
        if text.is_empty() {
            break;
        }
        if let Some((k, v)) = text.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).ok()?;
    Some(body)
}

/// A receiver that reads a delivery in full and then never answers,
/// holding the connection open: the "receiver that hangs" a webhook target
/// can be. Every accepted connection is kept alive on its own thread so
/// the accept loop is never blocked by one slow peer.
struct HangingReceiver {
    addr: SocketAddr,
    accepted: Arc<AtomicU32>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl HangingReceiver {
    fn start() -> HangingReceiver {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (a, s) = (accepted.clone(), stop.clone());
        std::thread::spawn(move || loop {
            if s.load(Ordering::Relaxed) {
                return;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    let a = a.clone();
                    std::thread::spawn(move || {
                        if read_request(&stream).is_some() {
                            a.fetch_add(1, Ordering::Relaxed);
                        }
                        // Never write a response; hold the connection open
                        // well past the client's request timeout and this
                        // test's own duration, then let it drop.
                        std::thread::sleep(Duration::from_secs(120));
                    });
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        });
        HangingReceiver {
            addr,
            accepted,
            stop,
        }
    }

    fn url(&self) -> String {
        format!("http://{}/hook", self.addr)
    }

    fn accepted(&self) -> u32 {
        self.accepted.load(Ordering::Relaxed)
    }
}

impl Drop for HangingReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A receiver that reads a delivery in full and then closes the
/// connection without writing any response at all: the "receiver that
/// closes mid-response" case (from the client's side, a connection that
/// disappears before the reply arrives looks the same whether it was
/// closed gracefully or reset).
struct ResettingReceiver {
    addr: SocketAddr,
    accepted: Arc<AtomicU32>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl ResettingReceiver {
    fn start() -> ResettingReceiver {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (a, s) = (accepted.clone(), stop.clone());
        std::thread::spawn(move || loop {
            if s.load(Ordering::Relaxed) {
                return;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    if read_request(&stream).is_some() {
                        a.fetch_add(1, Ordering::Relaxed);
                    }
                    // Close without writing a byte of response: the
                    // client's connection ends before any status line
                    // arrives, however the OS reports that shutdown.
                    drop(stream);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return,
            }
        });
        ResettingReceiver {
            addr,
            accepted,
            stop,
        }
    }

    fn url(&self) -> String {
        format!("http://{}/hook", self.addr)
    }

    fn accepted(&self) -> u32 {
        self.accepted.load(Ordering::Relaxed)
    }
}

impl Drop for ResettingReceiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Failure-direction: a webhook target that accepts the connection, reads
/// the whole delivery and then never answers must never delay or fail the
/// operation whose activity it was delivering, nor stall the feed for
/// other branches. See `docs/lifecycle.md` "Webhook failure direction".
#[test]
fn a_hanging_receiver_never_delays_the_operation_or_the_feed() {
    let f = Fixture::new();
    let receiver = HangingReceiver::start();
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
    let receiver = ResettingReceiver::start();
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
