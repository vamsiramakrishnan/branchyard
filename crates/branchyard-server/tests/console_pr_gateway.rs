//! PR router and claimed-worker contracts with fake authorization, queue and forge.
#![cfg(unix)]
#![allow(clippy::unwrap_used)] // tests: a panic reports the failed fixture or assertion

mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use branchyard::{Policy, PullRequestActivity, PullRequestRef, Yard};
use branchyard_client::api::{Operation, OperationState};
use branchyard_server::api::Caller;
use branchyard_server::config::Principal;
use branchyard_server::console_pr::{Action, AuthorizedBranch, Error, Service};
use branchyard_server::console_pr_gateway::{
    router, PrAccess, PrCommand, PrGateway, Queue, Resolver,
};
use branchyard_server::error::ApiError;
use branchyard_support::LockExt as _;
use serde_json::{json, Value};
use tower::ServiceExt as _;

const PATH: &str = "/v1/workspaces/workspace-a/sessions/session-a/pr";

/// Fake durable admission entry, including its original candidate-bound work.
struct Entry {
    identity: Value,
    command: PrCommand,
    operation: Operation,
}

/// An in-process router backed by a real engine candidate and fake forge tools.
struct Case {
    fixture: common::Fixture,
    yard: Yard,
    gateway: Arc<PrGateway>,
    resolver: Arc<Resolver>,
    allowed: Arc<AtomicBool>,
    rebound: Arc<AtomicBool>,
    entries: Arc<Mutex<BTreeMap<String, Entry>>>,
    runtime: tokio::runtime::Runtime,
}

/// An authenticated fixture principal with ordinary, independently checked scopes.
fn caller() -> Caller {
    let mut principal = Principal::default_for("operator");
    principal.tenant = "tenant-a".into();
    Caller(principal)
}

impl Case {
    /// Create real candidate state, live revocation hooks and callback-owned idempotency.
    fn new() -> Self {
        let fixture = common::Fixture::new();
        let yard = Yard::open(&fixture.root).unwrap();
        for name in ["review", "other"] {
            yard.task("WRITE report.txt=checked")
                .name(name)
                .harness("codex-acp")
                .command([common::fake_agent().display().to_string()])
                .policy(Policy::allow_all())
                .run()
                .unwrap();
        }
        yard.branch("review")
            .unwrap()
            .record_pull_request(PullRequestActivity::Opened(PullRequestRef {
                number: 17,
                url: "https://github.com/example/repo/pull/17".into(),
                head: "by/review".into(),
                base: Some("main".into()),
                draft: false,
            }))
            .unwrap();
        let allowed = Arc::new(AtomicBool::new(true));
        let rebound = Arc::new(AtomicBool::new(false));
        let grant = allowed.clone();
        let binding = rebound.clone();
        let target = yard.clone();
        let marker = fixture.root.join("revoked-during-command");
        let resolver: Arc<Resolver> = Arc::new(move |caller, workspace, session, access| {
            if caller.tenant() != "tenant-a"
                || caller.name() != "operator"
                || !grant.load(Ordering::Acquire)
                || marker.exists()
            {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "workspace_access_denied",
                    "workspace access was revoked",
                ));
            }
            if workspace != "workspace-a" || session != "session-a" {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "workspace_access_denied",
                    "workspace/session binding does not match",
                ));
            }
            let branch = if binding.load(Ordering::Acquire) {
                "other"
            } else {
                "review"
            };
            let permission = grant.clone();
            let current_binding = binding.clone();
            let marker = marker.clone();
            AuthorizedBranch::checked(
                target.clone(),
                branch,
                if access == PrAccess::Publish {
                    Action::Publish
                } else {
                    Action::Read
                },
                move |_| {
                    if !permission.load(Ordering::Acquire)
                        || marker.exists()
                        || current_binding.load(Ordering::Acquire) != (branch == "other")
                    {
                        Err(Error::denied("workspace access or session binding changed"))
                    } else {
                        Ok(())
                    }
                },
            )
            .map_err(|error| ApiError::new(StatusCode::FORBIDDEN, &error.code, error.message))
        });
        let entries = Arc::new(Mutex::new(BTreeMap::<String, Entry>::new()));
        let stored = entries.clone();
        let grant = allowed.clone();
        let queue: Arc<Queue> = Arc::new(move |caller, workspace, session, command, key| {
            if !grant.load(Ordering::Acquire) || caller.tenant() != "tenant-a" {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "workspace_access_denied",
                    "queue admission rechecks the grant",
                ));
            }
            let key = json!([caller.tenant(), caller.name(), workspace, session, key]).to_string();
            let identity = command.request_identity();
            let mut entries = stored.lock_recovering("fake PR queue");
            if let Some(entry) = entries.get(&key) {
                return if entry.identity == identity {
                    Ok(entry.operation.clone())
                } else {
                    Err(ApiError::new(
                        StatusCode::CONFLICT,
                        "idempotency_conflict",
                        "the key belongs to another PR request",
                    ))
                };
            }
            let operation: Operation = serde_json::from_value(json!({"id":"operation-1","repo":"managed","kind":"check","state":"queued","branches":["review"],"cursor":0,"created_at_ms":0})).unwrap();
            entries.insert(
                key,
                Entry {
                    identity,
                    command,
                    operation: operation.clone(),
                },
            );
            Ok(operation)
        });
        for (name, output) in [
            ("fake-by", r#"{"ok":true}"#),
            (
                "fake-gh",
                r#"{"comments":[{"body":"Check sources"}],"reviews":[{"state":"APPROVED"}],"statusCheckRollup":[]}"#,
            ),
        ] {
            let path = fixture.dir.join(name);
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" >> tool-arguments\nprintf '%s' '{output}'\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let service =
            Service::new(fixture.dir.join("fake-by")).with_gh(fixture.dir.join("fake-gh"));
        let gateway = Arc::new(PrGateway::new(service, resolver.clone(), Some(queue)));
        Self {
            fixture,
            yard,
            gateway,
            resolver,
            allowed,
            rebound,
            entries,
            runtime: tokio::runtime::Runtime::new().unwrap(),
        }
    }

    /// Exercise the real Axum router without allocating a socket or bypassing handlers.
    fn request(
        &self,
        gateway: Arc<PrGateway>,
        authenticated: Option<Caller>,
        method: &str,
        path: &str,
        body: Option<Value>,
        key: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(caller) = authenticated {
            request = request.extension(caller);
        }
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        let body = match body {
            Some(body) => {
                request = request.header("content-type", "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        self.runtime.block_on(async {
            let response = router::<()>(gateway)
                .oneshot(request.body(body).unwrap())
                .await
                .unwrap();
            let status = response.status();
            if status.is_success() {
                assert_eq!(response.headers()["cache-control"], "private, no-store");
                assert_eq!(response.headers()["x-content-type-options"], "nosniff");
            }
            let bytes = to_bytes(response.into_body(), 3 * 1024 * 1024)
                .await
                .unwrap();
            (status, serde_json::from_slice(&bytes).unwrap())
        })
    }

    /// Retrieve exactly the candidate-bound work admitted by the fake queue.
    fn command(&self) -> PrCommand {
        self.entries
            .lock_recovering("fake PR queue")
            .values()
            .next()
            .unwrap()
            .command
            .clone()
    }
}

#[test]
fn router_intersects_authentication_scope_tenant_membership_and_session_binding() {
    let case = Case::new();
    assert_eq!(
        case.request(case.gateway.clone(), None, "GET", PATH, None, None)
            .0,
        StatusCode::UNAUTHORIZED
    );
    let mut tenant = caller();
    tenant.0.tenant = "tenant-b".into();
    assert_eq!(
        case.request(case.gateway.clone(), Some(tenant), "GET", PATH, None, None)
            .0,
        StatusCode::FORBIDDEN
    );
    let mut viewer = caller();
    viewer.0.scopes.remove("merge");
    assert_eq!(
        case.request(
            case.gateway.clone(),
            Some(viewer),
            "POST",
            PATH,
            Some(json!({})),
            Some("no-merge")
        )
        .0,
        StatusCode::FORBIDDEN
    );
    let mut viewer = caller();
    viewer.0.scopes.remove("run");
    assert_eq!(
        case.request(
            case.gateway.clone(),
            Some(viewer),
            "POST",
            &format!("{PATH}/refresh"),
            Some(json!({})),
            Some("no-run")
        )
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        case.request(
            case.gateway.clone(),
            Some(caller()),
            "GET",
            &PATH.replace("session-a", "session-b"),
            None,
            None
        )
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, state) = case.request(
        case.gateway.clone(),
        Some(caller()),
        "GET",
        PATH,
        None,
        None,
    );
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state["pull_request"]["number"], 17);
    assert!(!state
        .to_string()
        .contains(case.fixture.root.to_str().unwrap()));
    case.allowed.store(false, Ordering::Release);
    assert_eq!(
        case.request(
            case.gateway.clone(),
            Some(caller()),
            "GET",
            PATH,
            None,
            None
        )
        .0,
        StatusCode::FORBIDDEN
    );
    assert!(case.entries.lock_recovering("fake PR queue").is_empty());
    assert!(!case.fixture.root.join("tool-arguments").exists());
}

#[test]
fn closed_requests_require_a_real_queue_bounded_body_and_idempotency_key() {
    let case = Case::new();
    for body in [
        json!({"command":"sh"}),
        json!({"path":"/outside","expected_candidate":"fake"}),
        json!({"title":"bad\ntitle"}),
    ] {
        assert_eq!(
            case.request(
                case.gateway.clone(),
                Some(caller()),
                "POST",
                PATH,
                Some(body),
                Some("bad-body")
            )
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        case.request(
            case.gateway.clone(),
            Some(caller()),
            "POST",
            PATH,
            Some(json!({})),
            None
        )
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        case.request(
            case.gateway.clone(),
            Some(caller()),
            "POST",
            PATH,
            Some(json!({})),
            Some(&"x".repeat(256))
        )
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        case.request(
            case.gateway.clone(),
            Some(caller()),
            "POST",
            PATH,
            Some(json!({"title":"x".repeat(5000)})),
            Some("too-large")
        )
        .0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        case.request(
            case.gateway.clone(),
            Some(caller()),
            "POST",
            &format!("{PATH}/refresh"),
            Some(json!({"url":"https://elsewhere"})),
            Some("bad-refresh")
        )
        .0,
        StatusCode::BAD_REQUEST
    );
    let gateway = Arc::new(PrGateway::new(
        Service::new(case.fixture.dir.join("must-not-start")),
        case.resolver.clone(),
        None,
    ));
    let (status, error) = case.request(
        gateway,
        Some(caller()),
        "POST",
        PATH,
        Some(json!({})),
        Some("no-queue"),
    );
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["error"]["code"], "pr_queue_unavailable");
    assert!(!case.fixture.root.join("tool-arguments").exists());
}

#[test]
fn candidate_bound_publication_runs_only_in_the_worker_with_literal_arguments() {
    let case = Case::new();
    let request = json!({"title":"$(touch injected) --force","base":"main","draft":true});
    let (status, operation) = case.request(
        case.gateway.clone(),
        Some(caller()),
        "POST",
        PATH,
        Some(request.clone()),
        Some("publish-once"),
    );
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(operation["id"], "operation-1");
    assert!(!case.fixture.root.join("tool-arguments").exists());
    let repeated = case.request(
        case.gateway.clone(),
        Some(caller()),
        "POST",
        PATH,
        Some(request),
        Some("publish-once"),
    );
    assert_eq!(repeated.1["id"], "operation-1");
    assert_eq!(case.entries.lock_recovering("fake PR queue").len(), 1);
    let conflict = case.request(
        case.gateway.clone(),
        Some(caller()),
        "POST",
        PATH,
        Some(json!({"title":"different"})),
        Some("publish-once"),
    );
    assert_eq!(conflict.0, StatusCode::CONFLICT);
    let command = case.command();
    let result = case
        .runtime
        .block_on(case.gateway.execute(
            &caller(),
            "workspace-a",
            "session-a",
            &command,
            "operation-1",
        ))
        .unwrap();
    assert_eq!(result.operation_id, "operation-1");
    assert_eq!(result.result["ok"], true);
    let args = std::fs::read_to_string(case.fixture.root.join("tool-arguments")).unwrap();
    assert!(
        args.contains("--title=$(touch injected) --force\n"),
        "{args}"
    );
    assert!(!case.fixture.root.join("injected").exists());
}

#[test]
fn queued_execution_rechecks_revocation_binding_candidate_and_operation_identity() {
    let case = Case::new();
    case.request(
        case.gateway.clone(),
        Some(caller()),
        "POST",
        PATH,
        Some(json!({})),
        Some("publish"),
    );
    let command = case.command();
    case.allowed.store(false, Ordering::Release);
    let error = case
        .runtime
        .block_on(case.gateway.execute(
            &caller(),
            "workspace-a",
            "session-a",
            &command,
            "operation-1",
        ))
        .unwrap_err();
    assert_eq!(error.operation_id, "operation-1");
    assert!(!error.error.unknown_effect);
    assert!(format!("{error}").contains("revoked"));
    case.allowed.store(true, Ordering::Release);
    case.rebound.store(true, Ordering::Release);
    let error = case
        .runtime
        .block_on(case.gateway.execute(
            &caller(),
            "workspace-a",
            "session-a",
            &command,
            "operation-1",
        ))
        .unwrap_err();
    assert_eq!(error.error.code, "workspace_binding_changed");
    case.rebound.store(false, Ordering::Release);
    case.yard
        .branch("review")
        .unwrap()
        .send(
            "WRITE report.txt=changed",
            branchyard::TaskOptions {
                harness: Some("codex-acp".into()),
                command: Some(vec![common::fake_agent().display().to_string()]),
                policy: Policy::allow_all(),
                ..branchyard::TaskOptions::default()
            },
        )
        .unwrap();
    let error = case
        .runtime
        .block_on(case.gateway.execute(
            &caller(),
            "workspace-a",
            "session-a",
            &command,
            "operation-1",
        ))
        .unwrap_err();
    assert_eq!(error.error.code, "stale_candidate");
    assert!(!error.error.unknown_effect);
    let error = case
        .runtime
        .block_on(case.gateway.execute(
            &caller(),
            "workspace-a",
            "session-a",
            &PrCommand::Refresh,
            "",
        ))
        .unwrap_err();
    assert_eq!(error.error.code, "invalid_operation");
    assert!(!case.fixture.root.join("tool-arguments").exists());
}

#[test]
fn uncertain_publication_stays_interrupted_in_the_callback_on_same_key_retry() {
    let case = Case::new();
    std::fs::write(
        case.fixture.dir.join("fake-by"),
        "#!/bin/sh\nprintf called >> forge-calls\nprintf malformed\n",
    )
    .unwrap();
    case.request(
        case.gateway.clone(),
        Some(caller()),
        "POST",
        PATH,
        Some(json!({})),
        Some("uncertain"),
    );
    let error = case
        .runtime
        .block_on(case.gateway.execute(
            &caller(),
            "workspace-a",
            "session-a",
            &case.command(),
            "operation-1",
        ))
        .unwrap_err();
    assert!(error.error.unknown_effect);
    assert_eq!(error.operation_id, "operation-1");
    case.entries
        .lock_recovering("fake PR queue")
        .values_mut()
        .next()
        .unwrap()
        .operation
        .state = OperationState::Interrupted;
    case.yard
        .branch("review")
        .unwrap()
        .send(
            "WRITE report.txt=another-candidate",
            branchyard::TaskOptions {
                harness: Some("codex-acp".into()),
                command: Some(vec![common::fake_agent().display().to_string()]),
                policy: Policy::allow_all(),
                ..branchyard::TaskOptions::default()
            },
        )
        .unwrap();
    let retry = case.request(
        case.gateway.clone(),
        Some(caller()),
        "POST",
        PATH,
        Some(json!({})),
        Some("uncertain"),
    );
    assert_eq!(retry.0, StatusCode::ACCEPTED);
    assert_eq!(retry.1["id"], "operation-1");
    assert_eq!(retry.1["state"], "interrupted");
    assert_eq!(
        std::fs::read_to_string(case.fixture.root.join("forge-calls")).unwrap(),
        "called"
    );
}

#[test]
fn refresh_uses_the_queue_and_post_effect_revocation_withholds_publication_results() {
    let case = Case::new();
    let queued = case.request(
        case.gateway.clone(),
        Some(caller()),
        "POST",
        &format!("{PATH}/refresh"),
        Some(json!({})),
        Some("refresh"),
    );
    assert_eq!(queued.0, StatusCode::ACCEPTED);
    assert_eq!(case.command(), PrCommand::Refresh);
    let result = case
        .runtime
        .block_on(case.gateway.execute(
            &caller(),
            "workspace-a",
            "session-a",
            &case.command(),
            "refresh-operation",
        ))
        .unwrap();
    assert_eq!(
        result.result["forge"]["comments"][0]["body"],
        "Check sources"
    );
    case.entries.lock_recovering("fake PR queue").clear();
    case.request(
        case.gateway.clone(),
        Some(caller()),
        "POST",
        PATH,
        Some(json!({})),
        Some("publish-revocation"),
    );
    std::fs::write(
        case.fixture.dir.join("fake-by"),
        "#!/bin/sh\n: > revoked-during-command\nprintf '%s' '{\"private-result\":true}'\n",
    )
    .unwrap();
    let error = case
        .runtime
        .block_on(case.gateway.execute(
            &caller(),
            "workspace-a",
            "session-a",
            &case.command(),
            "operation-1",
        ))
        .unwrap_err();
    assert!(error.error.unknown_effect);
    assert_eq!(error.error.code, "denied");
    assert!(!error.to_string().contains("private-result"));
}

#[test]
fn artifact_only_sessions_without_a_code_candidate_cannot_publish() {
    let case = Case::new();
    case.yard
        .task("say hello")
        .name("no-code")
        .harness("codex-acp")
        .command([common::fake_agent().display().to_string()])
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let yard = case.yard.clone();
    let resolver: Arc<Resolver> = Arc::new(move |_, _, _, _| {
        AuthorizedBranch::checked(yard.clone(), "no-code", Action::Publish, |_| Ok(()))
            .map_err(|error| ApiError::new(StatusCode::FORBIDDEN, &error.code, error.message))
    });
    let gateway = Arc::new(PrGateway::new(
        Service::new(case.fixture.dir.join("must-not-start")),
        resolver,
        None,
    ));
    let (status, error) = case.request(
        gateway,
        Some(caller()),
        "POST",
        PATH,
        Some(json!({})),
        Some("no-code"),
    );
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error["error"]["code"], "no_candidate");
    assert!(!case.fixture.root.join("tool-arguments").exists());
}

#[test]
fn refreshed_observations_are_bounded_after_combining_forge_and_engine_state() {
    let case = Case::new();
    std::fs::write(case.fixture.dir.join("fake-gh"), "#!/usr/bin/python3\nimport json\nprint(json.dumps({'comments':'x' * (2 * 1024 * 1024 - 100)}))\n").unwrap();
    case.request(
        case.gateway.clone(),
        Some(caller()),
        "POST",
        &format!("{PATH}/refresh"),
        Some(json!({})),
        Some("bounded-refresh"),
    );
    let error = case
        .runtime
        .block_on(case.gateway.execute(
            &caller(),
            "workspace-a",
            "session-a",
            &case.command(),
            "operation-1",
        ))
        .unwrap_err();
    assert_eq!(error.error.code, "pr_result_too_large");
    assert!(!error.error.unknown_effect);
    assert!(!error.error.message.contains("xxxxx"));
}

#[test]
fn a_binding_change_during_queue_admission_withholds_the_operation_response() {
    let case = Case::new();
    case.request(
        case.gateway.clone(),
        Some(caller()),
        "POST",
        PATH,
        Some(json!({})),
        Some("existing"),
    );
    let operation = case
        .entries
        .lock_recovering("fake PR queue")
        .values()
        .next()
        .unwrap()
        .operation
        .clone();
    let rebound = case.rebound.clone();
    let queue: Arc<Queue> = Arc::new(move |_, _, _, _, _| {
        rebound.store(true, Ordering::Release);
        Ok(operation.clone())
    });
    let gateway = Arc::new(PrGateway::new(
        Service::new(case.fixture.dir.join("must-not-start")),
        case.resolver.clone(),
        Some(queue),
    ));
    let (status, error) = case.request(
        gateway,
        Some(caller()),
        "POST",
        PATH,
        Some(json!({})),
        Some("existing"),
    );
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error["error"]["code"], "workspace_binding_changed");
    assert!(error.get("id").is_none());
    assert!(!case.fixture.root.join("tool-arguments").exists());
}
