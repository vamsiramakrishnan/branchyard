use axum::{
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use branchyard_sdk::*;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{sync::Mutex, task::JoinHandle};

struct Fixture {
    endpoint: String,
    task: JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve(router: Router) -> Fixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Fixture { endpoint, task }
}
fn client(f: &Fixture) -> Client {
    Client::with_options(
        &f.endpoint,
        "fixture-secret",
        ClientOptions {
            allow_loopback_http: true,
            timeout: Duration::from_secs(2),
        },
    )
    .unwrap()
}
fn command() -> Command {
    serde_json::from_str(include_str!("../../../examples/commands/create-task.json")).unwrap()
}
fn response(command: &Command) -> Receipt {
    Receipt {
        schema: Version::V1Alpha1,
        operation_id: command.operation_id,
        request_sha256: command.fingerprint().unwrap(),
        disposition: Disposition::Accepted,
    }
}

#[derive(Clone, Default)]
struct Store {
    operations: Arc<Mutex<BTreeMap<String, Operation>>>,
    calls: Arc<AtomicUsize>,
}
async fn accept(
    State(store): State<Store>,
    headers: HeaderMap,
    Json(c): Json<Command>,
) -> (StatusCode, Json<Value>) {
    store.calls.fetch_add(1, Ordering::SeqCst);
    assert_eq!(headers["authorization"], "Bearer fixture-secret");
    assert_eq!(headers["idempotency-key"], c.operation_id.to_string());
    let mut receipt = response(&c);
    assert_eq!(
        headers["x-branchyard-request-sha256"],
        receipt.request_sha256
    );
    let mut ops = store.operations.lock().await;
    let id = c.operation_id.to_string();
    if let Some(old) = ops.get(&id) {
        if old.request_sha256 != receipt.request_sha256 {
            return (StatusCode::CONFLICT, Json(json!({})));
        }
        receipt.disposition = Disposition::Replay;
    } else {
        ops.insert(
            id,
            Operation {
                schema: Version::V1Alpha1,
                operation_id: c.operation_id,
                request_sha256: receipt.request_sha256.clone(),
                state: OperationState::Pending,
                task_ids: vec![],
            },
        );
    }
    (
        StatusCode::ACCEPTED,
        Json(serde_json::to_value(receipt).unwrap()),
    )
}
async fn lookup(State(store): State<Store>, Path(id): Path<String>) -> (StatusCode, Json<Value>) {
    match store.operations.lock().await.get(&id) {
        Some(op) => (StatusCode::OK, Json(serde_json::to_value(op).unwrap())),
        None => (StatusCode::NOT_FOUND, Json(json!({}))),
    }
}
#[tokio::test]
async fn repeated_identity_replays_and_changed_input_conflicts() {
    let store = Store::default();
    let f = serve(
        Router::new()
            .route("/v1alpha1/commands", post(accept))
            .route("/v1alpha1/operations/{id}", get(lookup))
            .with_state(store.clone()),
    )
    .await;
    let c = command();
    let client = client(&f);
    assert_eq!(
        client.submit(&c).await.unwrap().disposition,
        Disposition::Accepted
    );
    assert_eq!(
        client.submit(&c).await.unwrap().disposition,
        Disposition::Replay
    );
    assert_eq!(
        client.reconcile(&c).await.unwrap().state,
        OperationState::Pending
    );
    let mut changed = c.clone();
    if let Action::CreateTask { spec, .. } = &mut changed.action {
        spec.goal = "changed".into();
    }
    assert!(matches!(
        client.submit(&changed).await,
        Err(Error::Http { status: 409 })
    ));
    assert!(matches!(
        client.reconcile(&changed).await,
        Err(Error::Protocol)
    ));
    assert_eq!(store.operations.lock().await.len(), 1);
    assert_eq!(store.calls.load(Ordering::SeqCst), 3);
}
#[tokio::test]
async fn timeout_after_admission_reconciles_without_resubmission() {
    let store = Store::default();
    let f = serve(
        Router::new()
            .route(
                "/v1alpha1/commands",
                post(|state, headers, command| async move {
                    let result = accept(state, headers, command).await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    result
                }),
            )
            .route("/v1alpha1/operations/{id}", get(lookup))
            .with_state(store.clone()),
    )
    .await;
    let client = Client::with_options(
        &f.endpoint,
        "fixture-secret",
        ClientOptions {
            allow_loopback_http: true,
            timeout: Duration::from_millis(150),
        },
    )
    .unwrap();
    let c = command();
    let result = client.submit(&c).await;
    assert!(
        matches!(result, Err(Error::SubmissionUnknown { operation_id, .. }) if operation_id == c.operation_id)
    );
    assert_eq!(
        client.reconcile(&c).await.unwrap().operation_id,
        c.operation_id
    );
    assert_eq!(store.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn dropped_submission_does_not_send_cancel() {
    let store = Store::default();
    let f = serve(
        Router::new()
            .route(
                "/v1alpha1/commands",
                post(|state, headers, command| async move {
                    let result = accept(state, headers, command).await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    result
                }),
            )
            .route("/v1alpha1/operations/{id}", get(lookup))
            .with_state(store.clone()),
    )
    .await;
    let c = command();
    let cc = c.clone();
    let first = client(&f);
    let pending = tokio::spawn(async move { first.submit(&cc).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while store.operations.lock().await.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    pending.abort();
    let _ = pending.await;
    assert_eq!(
        client(&f).reconcile(&c).await.unwrap().state,
        OperationState::Pending
    );
    assert_eq!(store.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn mismatched_or_malformed_receipt_is_uncertain() {
    for body in [
        "not json".to_string(),
        serde_json::to_string(&response(&Command {
            operation_id: OperationId::new(),
            ..command()
        }))
        .unwrap(),
    ] {
        let f = serve(Router::new().route(
            "/v1alpha1/commands",
            post(move || {
                let b = body.clone();
                async move { b }
            }),
        ))
        .await;
        assert!(matches!(
            client(&f).submit(&command()).await,
            Err(Error::SubmissionUnknown { .. })
        ));
    }
}
#[tokio::test]
async fn redirects_are_not_followed_or_retried() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let f = serve(
        Router::new()
            .route(
                "/v1alpha1/commands",
                post(|| async { (StatusCode::TEMPORARY_REDIRECT, [("location", "/trap")]) }),
            )
            .route(
                "/trap",
                post(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                    async { "trap" }
                }),
            ),
    )
    .await;
    assert!(matches!(
        client(&f).submit(&command()).await,
        Err(Error::SubmissionUnknown { .. })
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn oversized_chunked_read_and_future_version_fail_closed() {
    let f = serve(Router::new().route(
        "/v1alpha1/info",
        get(|| async {
            let stream = futures_util::stream::iter(vec![
                Ok::<_, std::io::Error>(vec![b' '; MAX_RESPONSE_BYTES]),
                Ok(vec![b' '; 1]),
            ]);
            Response::new(Body::from_stream(stream))
        }),
    ))
    .await;
    assert!(matches!(
        client(&f).info().await,
        Err(Error::ResponseTooLarge)
    ));
    let f = serve(Router::new().route("/v1alpha1/info", get(|| async { Json(json!({"schema":"branchyard/v2","execution_ready":true,"operations":[],"harness_profiles":[]})) }))).await;
    assert!(matches!(client(&f).info().await, Err(Error::Protocol)));
}
#[tokio::test]
async fn event_pages_cannot_skip_or_reorder_the_cursor() {
    let id = TaskId::new();
    for page in [
        json!({"schema":"branchyard/v1alpha1","task_id":id,"events":[],"next_after":12}),
        json!({"schema":"branchyard/v1alpha1","task_id":id,"events":[{"sequence":9,"kind":"task","summary":"done","artifacts":[]}],"next_after":9}),
    ] {
        let f = serve(Router::new().route(
            "/v1alpha1/tasks/{id}/events",
            get(move || {
                let p = page.clone();
                async { Json(p) }
            }),
        ))
        .await;
        assert!(matches!(
            client(&f).events(id, 10, 50).await,
            Err(Error::Protocol)
        ));
    }
}
#[tokio::test]
async fn cloned_client_supports_concurrent_independent_submissions() {
    let store = Store::default();
    let f = serve(
        Router::new()
            .route("/v1alpha1/commands", post(accept))
            .with_state(store.clone()),
    )
    .await;
    let client = client(&f);
    let mut pending = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let client = client.clone();
        let mut c = command();
        c.operation_id = OperationId::new();
        pending.spawn(async move { client.submit(&c).await.unwrap() });
    }
    while let Some(result) = pending.join_next().await {
        result.unwrap();
    }
    assert_eq!(store.operations.lock().await.len(), 32);
}
#[test]
fn insecure_endpoints_and_invalid_tokens_are_rejected_without_exposure() {
    for endpoint in [
        "http://example.com",
        "https://secret@example.com",
        "https://example.com?token=secret",
        "file:///tmp/service",
    ] {
        assert!(Client::new(endpoint, "secret").is_err());
    }
    assert!(Client::new("https://example.com", "secret\nheader").is_err());
    assert!(Client::with_options(
        "http://localhost:1234",
        "secret",
        ClientOptions {
            allow_loopback_http: true,
            ..Default::default()
        }
    )
    .is_err());
    let error = Client::new("https://secret@example.com", "secret")
        .err()
        .unwrap();
    assert!(!format!("{error:?} {error}").contains("secret"));
}

#[tokio::test]
async fn endpoint_prefix_is_preserved_and_wrong_read_identity_is_rejected() {
    let id = TaskId::new();
    let expected = Task {
        schema: Version::V1Alpha1,
        task_id: id,
        root_id: id,
        parent_id: None,
        revision: 0,
        graph_revision: 0,
        state: TaskState::Queued,
        effective_capabilities: Default::default(),
        artifacts: vec![],
    };
    let f = serve(Router::new().route(
        "/prefix/v1alpha1/tasks/{id}",
        get(move || {
            let task = expected.clone();
            async { Json(task) }
        }),
    ))
    .await;
    let client = Client::with_options(
        &format!("{}/prefix", f.endpoint),
        "fixture-secret",
        ClientOptions {
            allow_loopback_http: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(client.task(id).await.unwrap().task_id, id);
    assert!(matches!(
        client.task(TaskId::new()).await,
        Err(Error::Protocol)
    ));
}
