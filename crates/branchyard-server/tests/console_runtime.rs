//! Runtime HTTP adapters over a real local PTY and fake console authorization.
#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use branchyard::workspace_runtime::{RuntimeAction, TerminalOptions};
use branchyard::{TaskOptions, WorkspaceSpec, Yard};
use branchyard_server::api::Caller;
use branchyard_server::config::Principal;
use branchyard_server::console_runtime::{
    router, AuthorizedWorkspace, LifecycleQueue, Resolver, RuntimeGateway, RuntimeGatewayOptions,
};
use branchyard_server::error::ApiError;
use branchyard_support::LockExt as _;
use branchyard_testkit::wait;
use serde_json::{json, Value};
use tower::ServiceExt as _;

const SESSION: &str = "/v1/workspaces/workspace-a/sessions/session-a";

struct Case {
    gateway: Arc<RuntimeGateway>,
    resolver: Arc<Resolver>,
    allowed: Arc<AtomicBool>,
    yard: Yard,
    runtime: tokio::runtime::Runtime,
    _fixture: common::Fixture,
}

fn options(allow: bool, capacity: usize) -> RuntimeGatewayOptions {
    RuntimeGatewayOptions {
        allow_client_commands: allow,
        terminal: TerminalOptions {
            python: "/usr/bin/python3".into(),
            ..TerminalOptions::default()
        },
        max_terminals: capacity,
        ..RuntimeGatewayOptions::default()
    }
}

fn principal(name: &str) -> Caller {
    let mut principal = Principal::default_for(name);
    principal.tenant = "tenant-a".into();
    Caller(principal)
}

impl Case {
    fn new(allow: bool, capacity: usize, queue: Option<Arc<LifecycleQueue>>) -> Self {
        let fixture = common::Fixture::new();
        let yard = Yard::open(&fixture.root).unwrap();
        for name in ["session-a", "session-b"] {
            yard.task("say hello")
                .options(TaskOptions {
                    harness: Some("gemini-cli".into()),
                    command: Some(vec![common::fake_agent().display().to_string()]),
                    workspace: Some(WorkspaceSpec::default()),
                    ..TaskOptions::default()
                })
                .name(name)
                .run()
                .unwrap();
        }
        let allowed = Arc::new(AtomicBool::new(true));
        let permission = allowed.clone();
        let target = yard.clone();
        let resolver: Arc<Resolver> = Arc::new(move |caller, workspace, session, _| {
            if !permission.load(Ordering::Acquire) || caller.tenant() != "tenant-a" {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "workspace_access",
                    "workspace grant was revoked",
                ));
            }
            if !matches!(
                (workspace, session),
                ("workspace-a", "session-a") | ("workspace-b", "session-b")
            ) {
                return Err(ApiError::new(
                    StatusCode::NOT_FOUND,
                    "workspace_access",
                    "workspace session not found",
                ));
            }
            Ok(AuthorizedWorkspace {
                yard: target.clone(),
                branch: session.into(),
            })
        });
        let gateway = Arc::new(
            RuntimeGateway::new(options(allow, capacity), resolver.clone(), queue).unwrap(),
        );
        Self {
            gateway,
            resolver,
            allowed,
            yard,
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap(),
            _fixture: fixture,
        }
    }

    fn request(
        &self,
        caller: Option<Caller>,
        method: &str,
        path: &str,
        body: Option<Value>,
        key: Option<&str>,
    ) -> (StatusCode, Value) {
        self.request_to(self.gateway.clone(), caller, method, path, body, key)
    }

    fn request_to(
        &self,
        gateway: Arc<RuntimeGateway>,
        caller: Option<Caller>,
        method: &str,
        path: &str,
        body: Option<Value>,
        key: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(caller) = caller {
            request = request.extension(caller);
        }
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        let body = if let Some(body) = body {
            request = request.header("content-type", "application/json");
            Body::from(body.to_string())
        } else {
            Body::empty()
        };
        self.runtime.block_on(async {
            let response = router::<()>(gateway)
                .oneshot(request.body(body).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
            (status, serde_json::from_slice(&bytes).unwrap())
        })
    }

    fn open(&self, key: &str) -> String {
        let (status, handle) = self.request(
            Some(principal("tester")),
            "POST",
            &format!("{SESSION}/terminals"),
            Some(json!({})),
            Some(key),
        );
        assert_eq!(status, StatusCode::OK, "{handle}");
        assert_eq!(handle["persistent"], false);
        handle["id"].as_str().unwrap().into()
    }
}

#[test]
fn terminal_http_uses_real_pty_resize_replay_and_deduplicated_input() {
    let case = Case::new(true, 2, None);
    let id = case.open("open-once");
    assert_eq!(case.open("open-once"), id);
    let terminal = format!("{SESSION}/terminals/{id}");
    let (status, _) = case.request(
        Some(principal("tester")),
        "POST",
        &format!("{SESSION}/terminals"),
        Some(json!({"rows":42})),
        Some("open-once"),
    );
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = case.request(
        Some(principal("tester")),
        "POST",
        &format!("{terminal}/resize"),
        Some(json!({"rows":37,"columns":91})),
        None,
    );
    assert_eq!(status, StatusCode::OK);
    let data = STANDARD.encode(
        b"stty -echo; printf x >> duplicates; test -t 0 && printf '\\nPTY_%s\\n' true; stty size\n",
    );
    let input = json!({"sequence":1,"data":data});
    for _ in 0..2 {
        let (status, ack) = case.request(
            Some(principal("tester")),
            "POST",
            &format!("{terminal}/input"),
            Some(input.clone()),
            None,
        );
        assert_eq!(status, StatusCode::OK, "{ack}");
        assert_eq!(ack["next_sequence"], 2);
    }
    wait::until(
        "HTTP terminal output contains actual PTY dimensions",
        || {
            let (status, page) = case.request(
                Some(principal("tester")),
                "GET",
                &format!("{terminal}/output?after=0"),
                None,
                None,
            );
            assert_eq!(status, StatusCode::OK, "{page}");
            assert_eq!(page["encoding"], "base64");
            let bytes = STANDARD.decode(page["data"].as_str().unwrap()).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            text.contains("PTY_true") && text.contains("37 91")
        },
    );
    let path = case
        .yard
        .branch("session-a")
        .unwrap()
        .info()
        .worktree
        .join("duplicates");
    assert_eq!(std::fs::read(path).unwrap(), b"x");
    let (status, _) = case.request(
        Some(principal("tester")),
        "POST",
        &format!("{terminal}/input"),
        Some(json!({"sequence":3,"data":"eA=="})),
        None,
    );
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = case.request(
        Some(principal("tester")),
        "GET",
        &format!("{terminal}/output?after=18446744073709551615"),
        None,
        None,
    );
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    for _ in 0..2 {
        assert_eq!(
            case.request(
                Some(principal("tester")),
                "POST",
                &format!("{terminal}/close"),
                None,
                None
            )
            .0,
            StatusCode::OK
        );
    }
    case.yard
        .workspace_runtime("session-a")
        .unwrap()
        .close()
        .unwrap();
}

#[test]
fn revoked_members_lose_http_access_and_their_idle_pty_lease() {
    let case = Case::new(true, 2, None);
    let id = case.open("revocation");
    case.allowed.store(false, Ordering::Release);
    for (method, suffix, body) in [
        ("GET", "output", None),
        ("POST", "input", Some(json!({"sequence":1,"data":"eA=="}))),
        ("POST", "close", None),
    ] {
        let (status, _) = case.request(
            Some(principal("tester")),
            method,
            &format!("{SESSION}/terminals/{id}/{suffix}"),
            body,
            None,
        );
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    wait::until(
        "revoked HTTP terminal releases its branch without a client close",
        || {
            case.yard
                .workspace_runtime("session-a")
                .map(|runtime| runtime.close())
                .is_ok()
        },
    );
}

#[test]
fn capacity_owner_binding_and_restart_loss_are_explicit() {
    let case = Case::new(true, 1, None);
    let id = case.open("capacity");
    let (status, _) = case.request(
        Some(principal("other-editor")),
        "GET",
        &format!("{SESSION}/terminals/{id}/output"),
        None,
        None,
    );
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(
        case.request(
            Some(principal("tester")),
            "GET",
            &format!("/v1/workspaces/workspace-b/sessions/session-b/terminals/{id}/output"),
            None,
            None
        )
        .0,
        StatusCode::GONE
    );
    let mut foreign = principal("tester");
    foreign.0.tenant = "tenant-b".into();
    assert_eq!(
        case.request(
            Some(foreign),
            "GET",
            &format!("{SESSION}/terminals/{id}/output"),
            None,
            None
        )
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, body) = case.request(
        Some(principal("tester")),
        "POST",
        "/v1/workspaces/workspace-b/sessions/session-b/terminals",
        Some(json!({})),
        Some("another"),
    );
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    let restarted =
        Arc::new(RuntimeGateway::new(options(true, 1), case.resolver.clone(), None).unwrap());
    let (status, body) = case.request_to(
        restarted,
        Some(principal("tester")),
        "GET",
        &format!("{SESSION}/terminals/{id}/output"),
        None,
        None,
    );
    assert_eq!(status, StatusCode::GONE);
    assert_eq!(body["error"]["code"], "terminal_lost");
    case.gateway.close_all();
    case.yard
        .workspace_runtime("session-a")
        .unwrap()
        .close()
        .unwrap();
    assert_eq!(
        case.request(
            Some(principal("tester")),
            "POST",
            &format!("{SESSION}/terminals"),
            Some(json!({})),
            Some("after-shutdown")
        )
        .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[test]
fn authentication_scope_operator_policy_and_fixed_commands_are_required() {
    let case = Case::new(false, 2, None);
    assert_eq!(
        case.request(None, "GET", &format!("{SESSION}/runtime"), None, None)
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, _) = case.request(
        Some(principal("tester")),
        "POST",
        &format!("{SESSION}/terminals"),
        Some(json!({})),
        Some("disabled"),
    );
    assert_eq!(status, StatusCode::FORBIDDEN);
    let enabled =
        Arc::new(RuntimeGateway::new(options(true, 2), case.resolver.clone(), None).unwrap());
    let mut viewer = principal("viewer");
    viewer.0.scopes = ["read".into()].into_iter().collect();
    let (status, state) = case.request_to(
        enabled.clone(),
        Some(viewer.clone()),
        "GET",
        &format!("{SESSION}/runtime"),
        None,
        None,
    );
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state["terminal_available"], false);
    assert_eq!(state["preserves_sources"], true);
    assert_eq!(state["preserves_deliverables"], true);
    assert_eq!(
        case.request_to(
            enabled.clone(),
            Some(viewer),
            "POST",
            &format!("{SESSION}/terminals"),
            Some(json!({})),
            Some("viewer")
        )
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, _) = case.request_to(
        enabled.clone(),
        Some(principal("tester")),
        "POST",
        &format!("{SESSION}/terminals"),
        Some(json!({"shell":"/bin/evil","host":"other"})),
        Some("bad-command"),
    );
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = case.request_to(
        enabled,
        Some(principal("tester")),
        "POST",
        &format!("{SESSION}/terminals/unknown/input"),
        Some(json!({"sequence":1,"data":"A".repeat(25000)})),
        None,
    );
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[test]
fn foreign_worker_state_cannot_open_a_terminal_on_this_server() {
    let case = Case::new(true, 2, None);
    let db = rusqlite::Connection::open(case.yard.root().join(".branchyard/state.db")).unwrap();
    db.execute("UPDATE steps SET intent = json_set(intent, '$.host', 'another-worker-boot') WHERE branch = 'session-a' AND step = 'start'",[]).unwrap();
    let (status, state) = case.request(
        Some(principal("tester")),
        "GET",
        &format!("{SESSION}/runtime"),
        None,
        None,
    );
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state["terminal_available"], false);
    assert_eq!(state["runtime"]["owner_host"], "another-worker-boot");
    let (status, error) = case.request(
        Some(principal("tester")),
        "POST",
        &format!("{SESSION}/terminals"),
        Some(json!({})),
        Some("wrong-host"),
    );
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["error"]["code"], "unsupported");
}

#[test]
fn lifecycle_http_queues_work_and_rechecks_authority_before_worker_effects() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let queue: Arc<LifecycleQueue> = Arc::new(move |caller, workspace, session, action, key| {
        recorded.lock_recovering("queued runtime calls").push((
            caller.name().to_owned(),
            workspace.to_owned(),
            session.to_owned(),
            action,
            key.to_owned(),
        ));
        serde_json::from_value(json!({"id":"operation-1","repo":"managed","kind":"check","state":"queued","branches":["session-a"],"cursor":0,"created_at_ms":0}))
            .map_err(|error| ApiError::internal(error.to_string()))
    });
    let case = Case::new(true, 2, Some(queue));
    let path = format!("{SESSION}/runtime/actions/pause");
    assert_eq!(
        case.request(Some(principal("tester")), "POST", &path, None, None)
            .0,
        StatusCode::BAD_REQUEST
    );
    let (status, operation) = case.request(
        Some(principal("tester")),
        "POST",
        &path,
        None,
        Some("pause-once"),
    );
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(operation["id"], "operation-1");
    let no_queue =
        Arc::new(RuntimeGateway::new(options(true, 2), case.resolver.clone(), None).unwrap());
    let (status, error) = case.request_to(
        no_queue,
        Some(principal("tester")),
        "POST",
        &path,
        None,
        Some("no-queue"),
    );
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error["error"]["code"], "runtime_control_unavailable");
    assert_eq!(
        calls.lock_recovering("queued runtime calls")[0].3,
        RuntimeAction::Pause
    );
    assert_eq!(
        calls.lock_recovering("queued runtime calls")[0].4,
        "pause-once"
    );
    let error = case
        .gateway
        .execute_lifecycle(
            &principal("tester"),
            "workspace-a",
            "session-a",
            RuntimeAction::Pause,
            "operation-1",
        )
        .unwrap_err();
    assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY);
    case.allowed.store(false, Ordering::Release);
    assert_eq!(
        case.gateway
            .execute_lifecycle(
                &principal("tester"),
                "workspace-a",
                "session-a",
                RuntimeAction::Pause,
                "operation-1"
            )
            .unwrap_err()
            .status,
        StatusCode::FORBIDDEN
    );
}
