//! Adversarial direct-task diagnostics through the durable queue and HTTP API.

use super::*;
use crate::console::Projection;
use crate::error::ApiError;
use crate::ops::{Executor, Finished, NewOperation};
use crate::store::{AdmissionQuota, Idempotency, Lane, StoredOperation};
use branchyard_client::api::{ErrorBody, OperationKind, OperationResult, TaskRequest};

const KEY: &str = "/srv/operator/bridge.key";
const CA: &str = "/srv/operator/ca.pem";
const ENDPOINT: &str = "https://control.internal.example:8443";
const DIAGNOSTIC: &str = "connect timeout at https://control.internal.example:8443 using /srv/operator/bridge.key and /srv/operator/ca.pem; io_error unavailable";

/// A paused registry and registered repository. Only the synthetic executor
/// below runs; resolving this preset never contacts its endpoint.
fn configured_fixture() -> (Fixture, branchyard_testkit::Repo, Projection) {
    let mut fixture = Fixture::with(false, |config, _| {
        config.allow_providers.insert("substrate".into());
        config.secrets.insert(
            "io_error".into(),
            crate::config::parse_secret("io_error=OPERATOR_SECRET_SOURCE").unwrap(),
        );
        for name in [
            "failed",
            "status",
            "branches",
            "none",
            "lost",
            "approval",
            "edit_worktree",
            "edit-worktree",
            "state",
            "kind",
            "type",
            "reason",
            "detail",
            "input",
            "error",
            "code",
            "done",
            "landlock",
            "turn_ended",
            "permission_requested",
            "unrecognized",
        ] {
            config.secrets.insert(
                name.into(),
                crate::config::parse_secret(&format!("{name}=OPERATOR_SECRET_SOURCE")).unwrap(),
            );
        }
        config.execution_presets = serde_json::from_value(json!([{
            "id":"pool", "label":"Operator pool", "tenants":["default"],
            "provider":{"kind":"substrate", "endpoint":ENDPOINT,
                "router":"https://router.internal.example/{atespace}/{actor}/",
                "template":"harnesses", "key":KEY, "ca":CA},
            "harnesses":[{"harness":"gemini-cli"}], "secrets":["io_error", "failed", "status", "branches", "none", "lost", "approval", "edit_worktree", "edit-worktree", "state", "kind", "type", "reason", "detail", "input", "error", "code", "done", "landlock", "turn_ended", "permission_requested", "unrecognized"]
        }]))
        .unwrap();
    });
    let repo = branchyard_testkit::Repo::init("unused-by");
    let app = Arc::get_mut(&mut fixture.app).unwrap();
    app.config.repos.push(("code".into(), repo.root.clone()));
    let yard = crate::serve::open_yard(&app.config, "code", &repo.root).unwrap();
    crate::serve::configure_yard(&app.config, "code", &yard).unwrap();
    app.repos.insert(
        "code".into(),
        crate::api::RepoState {
            name: "code".into(),
            feed: Arc::new(crate::feed::Feed::open(yard.clone()).unwrap()),
            yard,
            wake: Arc::new(tokio::sync::Notify::new()),
        },
    );
    app.registry = Registry::open(
        Box::<MemoryStore>::default(),
        Options::new(vec!["code".into()]),
    )
    .unwrap();
    let (_, preset) = super::super::for_task(
        app,
        "default",
        &TaskRequest {
            prompt: "public task".into(),
            preset: Some("pool".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let projection = Projection::of_preset(&preset);
    (fixture, repo, projection)
}

fn adversarial_error() -> ErrorBody {
    ErrorBody {
        code: "io_error".into(),
        message: DIAGNOSTIC.into(),
        detail: Some(json!({CA: [ENDPOINT, {"source":"io_error", "attempt":3}],
            "cause":{"message":DIAGNOSTIC, "retryable":true}})),
    }
}

fn assert_private_values_hidden(value: &Value) {
    let text = value.to_string();
    for configured in [
        KEY,
        CA,
        "control.internal.example",
        "OPERATOR_SECRET_SOURCE",
    ] {
        assert!(
            !text.contains(configured),
            "{configured} disclosed in {text}"
        );
    }
    assert!(text.contains("connect timeout"), "diagnostic lost: {text}");
}

#[test]
fn direct_task_projection_preserves_public_codes_and_nested_diagnostics() {
    let (_fixture, _repo, projection) = configured_fixture();
    let restored: Projection =
        serde_json::from_slice(&serde_json::to_vec(&projection).unwrap()).unwrap();
    assert_eq!(restored, projection);
    let error = restored.error(adversarial_error());
    assert_eq!(
        error.code, "io_error",
        "configured names cannot alter public codes"
    );
    assert_private_values_hidden(&serde_json::to_value(&error).unwrap());
    let detail = error.detail.as_ref().unwrap();
    assert_eq!(detail["cause"]["retryable"], true);
    assert_eq!(
        detail["[preset pool: certificate authority]"][1]["attempt"],
        3
    );
    assert_eq!(
        detail["[preset pool: certificate authority]"][1]["source"],
        "[preset pool: secret]"
    );
    let projected = restored.api_error(ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        body: Box::new(adversarial_error()),
    });
    assert_eq!(projected.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(*projected.body, error);
    assert_eq!(
        restored.error(error.clone()),
        error,
        "retries keep placeholders stable"
    );
}

fn admission(key: &str, request: &TaskRequest) -> NewOperation {
    let canonical = serde_json::to_string(request).unwrap();
    NewOperation {
        repo: "code".into(),
        kind: OperationKind::Task,
        branches: Vec::new(),
        cursor: 0,
        locks: Vec::new(),
        idempotency: Some(Idempotency {
            caller: "owner".into(),
            key: key.into(),
            fingerprint: crate::api::fingerprint(&format!(
                "POST /v1/repos/code/tasks\n{canonical}"
            )),
        }),
        principal: Principal::default_for("owner"),
        creates: Vec::new(),
        quota: AdmissionQuota::default(),
        requires: Vec::new(),
        priority: 0,
        trace: None,
        lane: Lane::Normal,
    }
}

/// Produce exactly the engine-facing shapes whose text the API projects:
/// an error with nested detail, or a result containing failed branches.
struct DiagnosticExecutor;

impl Executor for DiagnosticExecutor {
    fn execute(&self, _: &StoredOperation, work: &Value) -> Finished {
        let result = if work["outcome"] == "result" {
            let branch = serde_json::from_value::<branchyard::BranchInfo>(json!({
                "name":"failed-root", "git_branch":"by/failed-root", "worktree":"/public/worktree",
                "prompt":"public task", "harness":"gemini-cli", "profile":"gemini-cli",
                "session":null, "parent":null, "base":"public-base", "candidate":null,
                "status":{"state":"failed", "reason":DIAGNOSTIC}, "turns":1,
                "cost_usd":null, "created_at":0,
                "stage":{"lane":"needs_you", "reason":"approval"},
                "guard":{"kind":"none", "reason":DIAGNOSTIC},
                "liveness":{"state":"lost", "heard_ms":0,"known_until_ms":0,"reason":DIAGNOSTIC}
            }))
            .unwrap();
            let mut child = branch.clone();
            child.name = "failed-child".into();
            child.parent = Some(branch.name.clone());
            Ok(OperationResult {
                branches: vec![branch],
                descendants: vec![child],
                runtime: Some(json!({"cause":{"message":DIAGNOSTIC}, CA:[ENDPOINT]})),
                ..Default::default()
            })
        } else {
            Err(adversarial_error())
        };
        Finished {
            result,
            end_cursor: Some(0),
            projection: Projection::default(),
            projection_tasks: Default::default(),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_projection_survives_storage_retries_and_response_serialization() {
    let (fixture, _repo, projection) = configured_fixture();
    let request = TaskRequest {
        prompt: "public task".into(),
        preset: Some("pool".into()),
        ..Default::default()
    };
    let mut admitted = Vec::new();
    for (key, outcome, snapshot) in [
        ("projected-error", "error", projection.clone()),
        ("projected-result", "result", projection.clone()),
        ("raw-error", "error", Projection::default()),
    ] {
        let (operation, replayed) = fixture
            .app
            .registry
            .submit_projected(
                admission(key, &request),
                json!({"outcome":outcome}),
                snapshot,
            )
            .unwrap();
        assert!(!replayed);
        admitted.push((key, operation.id));
    }
    for stored in fixture.app.registry.unfinished("default").unwrap() {
        let restored: StoredOperation =
            serde_json::from_slice(&serde_json::to_vec(&stored).unwrap()).unwrap();
        assert_eq!(
            stored, restored,
            "private snapshot survives durable serialization"
        );
    }
    let (status, queued) = fixture
        .json(
            Method::GET,
            "/v1/repos/code/operations",
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(queued["operations"].as_array().unwrap().len(), 3);
    assert!(!queued.to_string().contains("projection"), "{queued}");
    fixture
        .app
        .registry
        .start(Arc::new(DiagnosticExecutor))
        .unwrap();
    let mut done = Vec::new();
    for (key, id) in &admitted {
        done.push(fixture.finished(id));
        for path in [
            format!("/v1/operations/{id}"),
            format!("/v1/operations?idempotency_key={key}"),
        ] {
            let (status, value) = fixture
                .json(Method::GET, &path, OWNER, None, Value::Null)
                .await;
            assert_eq!(status, StatusCode::OK, "{path}: {value}");
            assert_eq!(value["id"], *id);
            assert!(!value.to_string().contains("projection"), "{value}");
            if *key == "raw-error" {
                assert_eq!(value["error"]["message"], DIAGNOSTIC);
            } else {
                assert_private_values_hidden(&value);
                if *key == "projected-error" {
                    assert_eq!(value["error"]["code"], "io_error");
                } else {
                    assert_eq!(value["result"]["branches"][0]["status"]["state"], "failed");
                    assert_eq!(value["result"]["descendants"][0]["name"], "failed-child");
                }
            }
        }
        for token in [FOREIGN, RESTRICTED] {
            let (status, denied) = fixture
                .json(
                    Method::GET,
                    &format!("/v1/operations/{id}"),
                    token,
                    None,
                    Value::Null,
                )
                .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{token}: {denied}");
            assert_eq!(denied["error"]["code"], "unknown_operation");
        }
        let (status, replay) = fixture
            .json(
                Method::POST,
                "/v1/repos/code/tasks",
                OWNER,
                Some(key),
                serde_json::to_value(&request).unwrap(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{replay}");
        assert_eq!(replay["id"], *id);
        if *key != "raw-error" {
            assert_private_values_hidden(&replay);
        }
    }
    let response = crate::api::operation_list(&fixture.app, done).unwrap();
    let listed: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    for operation in listed["operations"].as_array().unwrap() {
        if operation["id"] != admitted[2].1 {
            assert_private_values_hidden(operation);
        } else {
            assert_eq!(operation["error"]["message"], DIAGNOSTIC);
        }
    }
    let id = &admitted[0].1;
    assert!(
        fixture
            .app
            .registry
            .operation_projection(id, "other-repository")
            .unwrap()
            .is_empty(),
        "operation identity cannot borrow configuration from another repository"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_direct_tasks_recheck_the_admitting_principal_and_preset() {
    for change in [
        "removed",
        "other-tenant",
        "no-run-scope",
        "other-repository",
    ] {
        let (mut fixture, _repo, _projection) = configured_fixture();
        let mut principal = Principal::default_for("developer");
        principal.scopes = ["read".into(), "run".into()].into_iter().collect();
        let request = TaskRequest {
            prompt: "WRITE must-not-run.txt=local fallback".into(),
            name: Some("must-not-run".into()),
            preset: Some("pool".into()),
            ..Default::default()
        };
        let (operation, replayed) = crate::api::admit_task(
            &fixture.app,
            &fixture.app.repos["code"],
            &Caller::new(principal.clone()),
            None,
            request.clone(),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(!replayed);
        let snapshot = fixture
            .app
            .registry
            .operation_projection(&operation.id, "code")
            .unwrap();
        let mut stored: StoredOperation = serde_json::from_value(json!({
            "operation":operation, "principal":principal, "tenant":"default",
            "projection":snapshot
        }))
        .unwrap();
        let app = Arc::get_mut(&mut fixture.app).unwrap();
        let expected = match change {
            "removed" => {
                app.config.execution_presets.clear();
                "unknown_preset"
            }
            "other-tenant" => {
                app.config.execution_presets[0].tenants = Some(vec!["foreign".into()]);
                "unknown_preset"
            }
            "no-run-scope" => {
                stored.principal.as_mut().unwrap().scopes = ["read".into()].into_iter().collect();
                "scope_required"
            }
            "other-repository" => {
                stored.principal.as_mut().unwrap().repos =
                    Some(["other".into()].into_iter().collect());
                "repo_not_allowed"
            }
            _ => unreachable!(),
        };
        let stored: StoredOperation =
            serde_json::from_slice(&serde_json::to_vec(&stored).unwrap()).unwrap();
        let work = crate::work::Work::Task {
            request,
            trigger: None,
        }
        .to_value()
        .unwrap();
        let work: Value = serde_json::from_slice(&serde_json::to_vec(&work).unwrap()).unwrap();
        let finished = crate::work::AppExecutor(fixture.app.clone()).execute(&stored, &work);
        if matches!(change, "no-run-scope" | "other-repository") {
            assert_eq!(
                finished.end_cursor,
                Some(stored.operation.cursor),
                "{change}: a refusal before execution owns no feed entries"
            );
        }
        assert_eq!(finished.result.unwrap_err().code, expected, "{change}");
        assert!(
            fixture.app.repos["code"]
                .yard
                .branches()
                .unwrap()
                .is_empty(),
            "{change}: denied work created a branch"
        );
    }
}

/// Read only through the feed head announced at connection time. A refused
/// operation must not cause this body to end before unrelated entries arrive.
async fn feed_through_head(
    fixture: &Fixture,
    last_event_id: Option<u64>,
) -> (u64, Vec<branchyard_client::api::FeedEntry>) {
    use axum::body::HttpBody as _;

    let mut request = Request::builder()
        .uri("/v1/repos/code/events/stream?cursor=0")
        .header("authorization", format!("Bearer {OWNER}"));
    if let Some(cursor) = last_event_id {
        request = request.header("last-event-id", cursor.to_string());
    }
    let response = crate::api::router(fixture.app.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let mut text = String::new();
    loop {
        let data: Vec<Value> = text
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|data| serde_json::from_str(data.trim()).unwrap())
            .collect();
        let head = data.iter().find_map(|data| data["head"].as_u64());
        let last = text
            .lines()
            .filter_map(|line| line.strip_prefix("id:"))
            .map(|id| id.trim().parse::<u64>().unwrap())
            .max();
        if let Some(head) = head.filter(|head| last.is_some_and(|last| last >= *head)) {
            let entries = data
                .into_iter()
                .filter(|data| data.get("seq").is_some())
                .map(|entry| serde_json::from_value(entry).unwrap())
                .collect();
            return (head, entries);
        }
        let frame = tokio::time::timeout(
            Duration::from_secs(30),
            std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)),
        )
        .await
        .expect("unrelated activity stalled before the announced feed head")
        .expect("a refused task terminated unrelated activity delivery")
        .unwrap();
        if let Ok(bytes) = frame.into_data() {
            text.push_str(std::str::from_utf8(&bytes).unwrap());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_authorization_refusal_does_not_poison_unrelated_feed_reconnects() {
    let (mut fixture, _repo, projection) = configured_fixture();
    Arc::get_mut(&mut fixture.app)
        .unwrap()
        .config
        .execution_presets[0]
        .tenants = None;

    // Seed real native activity so the task's empty interval must use its
    // admitted cursor rather than assume a new repository starts at zero.
    let seed = TaskRequest {
        prompt: "WRITE seed.txt=ran".into(),
        name: Some("before-refusal".into()),
        harness: Some("gemini-cli".into()),
        isolated: true,
        ..Default::default()
    };
    let repo = &fixture.app.repos["code"];
    let options = crate::work::task_options(&fixture.app, repo, &seed).unwrap();
    repo.yard.task(seed.prompt).options(options).run().unwrap();
    assert!(repo.feed.sync().unwrap() > 0);

    let mut principal = Principal::default_for("developer");
    principal.tenant = "revoked".into();
    principal.scopes = ["read".into(), "run".into()].into_iter().collect();
    let request = TaskRequest {
        prompt: "WRITE must-not-run.txt=local fallback".into(),
        name: Some("refused-before-execution".into()),
        preset: Some("pool".into()),
        ..Default::default()
    };
    let (operation, replayed) = crate::api::admit_task(
        &fixture.app,
        repo,
        &Caller::new(principal),
        None,
        request,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(!replayed);
    assert!(operation.cursor > 0);

    // The durable request was authorized when admitted. Its tenant loses
    // repository access while queued; the default tenant can still run.
    Arc::get_mut(&mut fixture.app)
        .unwrap()
        .config
        .tenants
        .insert(
            "revoked".into(),
            crate::config::TenantPolicy {
                repos: Some(Default::default()),
                ..Default::default()
            },
        );
    fixture
        .app
        .registry
        .start(Arc::new(crate::work::AppExecutor(fixture.app.clone())))
        .unwrap();
    let refused = fixture.finished(&operation.id);
    assert_eq!(
        refused.state,
        branchyard_client::api::OperationState::Failed
    );
    assert_eq!(refused.error.unwrap().code, "repo_not_allowed");
    assert_eq!(refused.end_cursor, Some(operation.cursor));
    assert_eq!(
        fixture
            .app
            .registry
            .operation_projection(&operation.id, "code")
            .unwrap(),
        projection,
        "the failed operation retains its private diagnostic snapshot"
    );
    assert!(fixture
        .app
        .registry
        .feed_projection_candidates("code", operation.cursor + 1)
        .unwrap()
        .is_empty());
    assert!(matches!(
        fixture.app.repos["code"]
            .yard
            .branch("refused-before-execution"),
        Err(branchyard::Error::UnknownBranch(_))
    ));

    let prompt = format!("WRITE raw.txt=ran\n{DIAGNOSTIC}");
    let (status, admitted) = fixture
        .json(
            Method::POST,
            "/v1/repos/code/tasks",
            OWNER,
            Some("unrelated-raw-after-refusal"),
            json!({"prompt":prompt,"name":"unrelated-raw","harness":"gemini-cli","isolated":true}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{admitted}");
    let raw = fixture.finished(admitted["id"].as_str().unwrap());
    assert_eq!(
        raw.state,
        branchyard_client::api::OperationState::Succeeded,
        "{raw:?}"
    );

    let (head, entries) = feed_through_head(&fixture, None).await;
    assert!(head > operation.cursor);
    assert!(entries.iter().any(|entry| entry.branch == "unrelated-raw"
        && matches!(&entry.activity, branchyard::Activity::Prompt(text) if text.contains(DIAGNOSTIC))),
        "a refused preset must not project an unrelated raw task's public diagnostics");
    let (reconnected_head, resumed) = feed_through_head(&fixture, Some(operation.cursor)).await;
    assert_eq!(reconnected_head, head);
    assert_eq!(
        resumed,
        entries
            .into_iter()
            .filter(|entry| entry.seq > operation.cursor)
            .collect::<Vec<_>>(),
        "Last-Event-ID resumes every unrelated event after the refusal"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_executing_task_projects_the_worker_configuration_and_keeps_admission_values() {
    let (mut fixture, _repo, _projection) = configured_fixture();
    let request = TaskRequest {
        prompt: "WRITE must-not-run.txt=local fallback".into(),
        name: Some("changed-preset".into()),
        preset: Some("pool".into()),
        ..Default::default()
    };
    let principal = Principal::default_for("owner");
    let (operation, _) = crate::api::admit_task(
        &fixture.app,
        &fixture.app.repos["code"],
        &Caller::new(principal.clone()),
        None,
        request.clone(),
        None,
        None,
    )
    .await
    .unwrap();
    let snapshot = fixture
        .app
        .registry
        .operation_projection(&operation.id, "code")
        .unwrap();
    let stored: StoredOperation = serde_json::from_value(json!({
        "operation":operation, "principal":principal, "tenant":"default", "projection":snapshot
    }))
    .unwrap();
    let app = Arc::get_mut(&mut fixture.app).unwrap();
    let worker_key = app
        .config
        .data_dir
        .join("worker-private/missing-bridge.key");
    app.config.execution_presets[0].provider = serde_json::from_value(json!({
        "kind":"substrate", "endpoint":"https://worker.internal.example:8443",
        "router":"https://worker-router.internal.example/{atespace}/{actor}/",
        "template":"harnesses", "key":worker_key
    }))
    .unwrap();
    // The missing bridge key must fail before any provider request. There
    // are no provisioned secrets to fail earlier in the engine.
    app.config.execution_presets[0].secrets.clear();
    let work = crate::work::Work::Task {
        request,
        trigger: None,
    }
    .to_value()
    .unwrap();
    let finished = crate::work::AppExecutor(fixture.app.clone()).execute(&stored, &work);
    let error = finished.result.unwrap_err();
    assert!(
        !error.message.contains(&worker_key.display().to_string()),
        "{error:?}"
    );
    assert!(
        error.message.contains("[preset pool: bridge key]"),
        "{error:?}"
    );
    let mut diagnostic = json!({"admission":DIAGNOSTIC,
        "worker":format!("{} at worker.internal.example:8443", worker_key.display())});
    finished.projection.apply(&mut diagnostic);
    assert_private_values_hidden(&diagnostic);
    assert!(
        !diagnostic.to_string().contains("worker.internal.example"),
        "{diagnostic}"
    );
    assert!(
        !diagnostic
            .to_string()
            .contains(&worker_key.display().to_string()),
        "{diagnostic}"
    );
    assert!(
        fixture.app.repos["code"]
            .yard
            .branches()
            .unwrap()
            .is_empty(),
        "a provider configuration failure must not fall back to a local harness"
    );
}

#[tokio::test]
async fn standalone_branch_event_and_wait_records_preserve_schema_under_name_collisions() {
    let (_fixture, _repo, projection) = configured_fixture();
    let branch: branchyard::BranchInfo = serde_json::from_value(json!({
        "name":"public-branch", "git_branch":"by/public-branch", "worktree":"/public/worktree",
        "prompt":"public task", "harness":"gemini-cli", "profile":"gemini-cli",
        "session":null, "parent":null, "base":"public-base", "candidate":null,
        "status":{"state":"failed", "reason":DIAGNOSTIC}, "turns":1,
        "cost_usd":null, "created_at":0,
        "stage":{"lane":"done", "reason":"failed"},
        "guard":{"kind":"landlock", "preset":"edit-worktree", "abi":1, "writable":[KEY]},
        "liveness":{"state":"lost", "heard_ms":0,"known_until_ms":0,"reason":DIAGNOSTIC}
    }))
    .unwrap();
    let response = Projection::respond_at(&branch, &[(String::new(), projection.clone())]).unwrap();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let shown: branchyard::BranchInfo = serde_json::from_slice(&bytes).unwrap();
    assert!(matches!(
        shown.status,
        branchyard::BranchStatus::Failed { .. }
    ));
    assert_eq!(shown.stage, branch.stage);
    assert_eq!(
        shown.liveness.as_ref().unwrap().state,
        branchyard::LivenessState::Lost
    );
    assert!(matches!(
        shown.guard,
        Some(branchyard::Guard::Landlock {
            preset: branchyard::PolicyPreset::EditWorktree,
            ..
        })
    ));
    assert_private_values_hidden(&serde_json::to_value(&shown).unwrap());

    // A wait projects its individual inspection at the handler's pointer,
    // through the same boundary as the corresponding standalone record.
    let response = Projection::respond_at(
        json!({"settled":[branch], "pending":[], "timed_out":false}),
        &[("/settled/0".into(), projection.clone())],
    )
    .unwrap();
    let waited: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let waited_branch: branchyard::BranchInfo =
        serde_json::from_value(waited["settled"][0].clone()).unwrap();
    assert_eq!(waited_branch, shown);

    let events = vec![
        branchyard::RecordedEvent {
            at_ms: 0,
            activity: branchyard::Activity::Status(branchyard::BranchStatus::Failed {
                reason: DIAGNOSTIC.into(),
            }),
        },
        branchyard::RecordedEvent {
            at_ms: 1,
            activity: branchyard::Activity::Harness(branchyard::Event::TurnEnded {
                turn: 1,
                outcome: branchyard::TurnOutcome::Failed {
                    message: DIAGNOSTIC.into(),
                },
            }),
        },
        branchyard::RecordedEvent {
            at_ms: 2,
            activity: branchyard::Activity::Harness(branchyard::Event::PermissionRequested {
                turn: Some(1),
                request: branchyard::PermissionRequest {
                    key: branchyard::PermissionKey("public-permission".into()),
                    tool: "public-tool".into(),
                    input: json!({CA:[ENDPOINT], "kind":"failed"}),
                },
            }),
        },
        branchyard::RecordedEvent {
            at_ms: 3,
            activity: branchyard::Activity::Harness(branchyard::Event::Unrecognized {
                kind: KEY.into(),
            }),
        },
        branchyard::RecordedEvent {
            at_ms: 4,
            activity: branchyard::Activity::Harness(branchyard::Event::HarnessTaskStarted {
                background: false,
                task: branchyard::HarnessTask {
                    task_id: "public-task".into(),
                    kind: Some(KEY.into()),
                    description: DIAGNOSTIC.into(),
                },
            }),
        },
        branchyard::RecordedEvent {
            at_ms: 5,
            activity: branchyard::Activity::Result {
                value: json!({CA:ENDPOINT, "status":"failed"}),
            },
        },
    ];
    let response = Projection::respond_at(events, &[(String::new(), projection.clone())]).unwrap();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let shown: Vec<branchyard::RecordedEvent> = serde_json::from_slice(&bytes).unwrap();
    assert_private_values_hidden(&serde_json::to_value(&shown).unwrap());
    match &shown[2].activity {
        branchyard::Activity::Harness(branchyard::Event::PermissionRequested {
            request, ..
        }) => {
            assert_eq!(
                request.input["[preset pool: certificate authority]"][0],
                "[preset pool: control endpoint]"
            );
            assert_eq!(
                request.input["[preset pool: secret]"],
                "[preset pool: secret]"
            );
        }
        other => panic!("permission event lost its schema: {other:?}"),
    }
    let mut feed = serde_json::to_value(branchyard_client::api::FeedEntry {
        seq: 1,
        branch: "public-branch".into(),
        at_ms: 0,
        activity: branchyard::Activity::Harness(branchyard::Event::TurnEnded {
            turn: 1,
            outcome: branchyard::TurnOutcome::Failed {
                message: DIAGNOSTIC.into(),
            },
        }),
    })
    .unwrap();
    projection.apply_record(&mut feed);
    serde_json::from_value::<branchyard_client::api::FeedEntry>(feed.clone()).unwrap();
    assert_private_values_hidden(&feed);
}
