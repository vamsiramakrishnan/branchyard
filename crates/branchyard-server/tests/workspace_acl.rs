//! Hermetic legacy-path regression checks for durable workspace membership.
#![allow(clippy::unwrap_used)] // tests: failures report invalid test setup
mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

use branchyard_server::console_store::{Collection, Mutation, Record};
use branchyard_server::store::{OperationStore, SqliteStore};
use common::{get, raw, run, task, Fixture, Server, TOKEN};
use serde_json::json;

fn record(collection: Collection, id: &str, body: serde_json::Value) -> Mutation {
    Mutation::Put {
        record: Record {
            collection,
            id: id.into(),
            tenant: "default".into(),
            revision: 1,
            body,
        },
        expected: None,
    }
}

#[test]
fn revoked_members_cannot_bypass_through_legacy_paths_or_open_streams() {
    let fixture = Fixture::new();
    let server = Server::start(fixture.config());
    let operation = run(&server.client(), &task("WRITE a.txt=workspace", "private"));
    let store = SqliteStore::open(fixture.data.join("state.db"), None).unwrap();
    store
        .console_mutate(&[
            record(
                Collection::Workspaces,
                "workspace",
                json!({"repo":"app", "task_id":"task"}),
            ),
            record(
                Collection::Bindings,
                "app:private",
                json!({"workspace_id":"workspace","repo":"app","branch":"private"}),
            ),
            record(
                Collection::Members,
                "workspace:tester",
                json!({"workspace_id":"workspace","subject":"tester","role":"owner"}),
            ),
        ])
        .unwrap();
    assert_eq!(
        raw(
            server.addr,
            &get("/v1/repos/app/branches/private", Some(TOKEN))
        )
        .0,
        200
    );

    let mut stream = TcpStream::connect(server.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(stream, "GET /v1/repos/app/events/stream HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\n\r\n").unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("200"));
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
    }
    store
        .console_mutate(&[Mutation::Delete {
            collection: Collection::Members,
            id: "workspace:tester".into(),
            tenant: "default".into(),
            expected: 1,
        }])
        .unwrap();
    for path in [
        "/v1/repos/app/branches/private".to_owned(),
        "/v1/repos/app/branches/private/events".to_owned(),
        "/v1/repos/app/branches/private/artifacts".to_owned(),
        "/v1/repos/app/branches/private/artifacts/missing/content".to_owned(),
        "/v1/repos/app/task-records".to_owned(),
        "/v1/repos/app/events/stream".to_owned(),
        format!("/v1/operations/{}", operation.id),
    ] {
        assert_eq!(raw(server.addr, &get(&path, Some(TOKEN))).0, 403, "{path}");
    }
    // EOF or the terminating chunk proves the already-open stream ended after revocation.
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap() == 0 || line == "0\r\n" {
            break;
        }
    }
}

#[test]
fn incomplete_binding_and_cross_tenant_aggregate_access_fail_closed() {
    let fixture = Fixture::new();
    let server = Server::start(fixture.config());
    run(&server.client(), &task("WRITE a.txt=workspace", "private"));
    let store = SqliteStore::open(fixture.data.join("state.db"), None).unwrap();
    store
        .console_mutate(&[
            record(
                Collection::Bindings,
                "app:private",
                json!({"repo":"app","branch":"private"}),
            ),
            Mutation::Put {
                record: Record {
                    collection: Collection::Workspaces,
                    id: "foreign".into(),
                    tenant: "foreign".into(),
                    revision: 1,
                    body: json!({"repo":"app","task_id":"task"}),
                },
                expected: None,
            },
        ])
        .unwrap();
    for path in [
        "/v1/repos/app/branches/private",
        "/v1/repos/app/branches",
        "/v1/repos/app/task-records",
        "/v1/repos/app/events/stream",
    ] {
        assert_eq!(raw(server.addr, &get(path, Some(TOKEN))).0, 403, "{path}");
    }
    assert_eq!(
        raw(
            server.addr,
            &get("/v1/repos/__console_managed__/task-records", Some(TOKEN))
        )
        .0,
        403
    );
}

#[test]
fn delegated_device_comments_attribute_verified_subject_and_acting_credential() {
    use branchyard_server::companion::{link, store::SqliteCompanion};
    use branchyard_server::config::Principal;
    let fixture = Fixture::new();
    let mut config = fixture.config();
    config.app.enabled = true;
    let server = Server::start(config.clone());
    let store = SqliteStore::open(fixture.data.join("state.db"), None).unwrap();
    store
        .console_mutate(&[
            record(
                Collection::Workspaces,
                "workspace",
                json!({"repo":"app","task_id":"task"}),
            ),
            record(
                Collection::Members,
                "workspace:tester",
                json!({"workspace_id":"workspace","subject":"tester","role":"owner"}),
            ),
        ])
        .unwrap();
    let companion = SqliteCompanion::open(&fixture.data.join("state.db")).unwrap();
    let request = link::LinkRequest {
        name: Some("phone-device".into()),
        tenant: "default".into(),
        scopes: vec!["read".into(), "run".into()],
        repos: Some(vec!["app".into()]),
        ttl: None,
        code_ttl: None,
        public_url: Some("https://console.example".into()),
        qr: false,
    };
    let (_, code) = link::create_delegated(
        &config,
        &companion,
        &request,
        &Principal::default_for("tester"),
        branchyard_support::time::now_ms(),
    )
    .unwrap();
    let (status, _, body) = raw(
        server.addr,
        &common::post("/app/pair", None, "", &json!({"code":code}).to_string()),
    );
    assert_eq!(status, 200);
    let paired: serde_json::Value = serde_json::from_str(&body).unwrap();
    let (status, _, body) = raw(
        server.addr,
        &common::post(
            "/v1/workspaces/workspace/comments",
            paired["token"].as_str(),
            "Idempotency-Key: attributed-comment\r\n",
            &json!({"revision":1,"text":"Reviewed the immutable source."}).to_string(),
        ),
    );
    assert_eq!(status, 200, "{body}");
    let comment: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(comment["subject"], "tester");
    assert_eq!(comment["credential"], "phone-device");
    let (status, _, body) = raw(
        server.addr,
        &common::post(
            "/v1/workspaces/workspace/comments",
            paired["token"].as_str(),
            "Idempotency-Key: stale-comment\r\n",
            &json!({"revision":1,"text":"stale"}).to_string(),
        ),
    );
    assert_eq!(status, 409, "{body}");
    let member = store
        .console_get(Collection::Members, "workspace:tester")
        .unwrap()
        .unwrap();
    store
        .console_mutate(&[Mutation::Delete {
            collection: Collection::Members,
            id: member.id,
            tenant: member.tenant,
            expected: member.revision,
        }])
        .unwrap();
    assert_eq!(
        raw(
            server.addr,
            &get(
                "/v1/workspaces/workspace/comments",
                paired["token"].as_str()
            )
        )
        .0,
        403
    );
}

#[test]
fn typed_client_reads_durable_console_metadata_and_native_conversations() {
    use branchyard_client::workspace_api::{
        CreateProjectRequest, CreateWorkspaceRequest, MessageRequest, ProjectSource, ResultMode,
        SessionAccess, StartSessionRequest,
    };
    let fixture = Fixture::new();
    let server = Server::start(fixture.config());
    let client = server.client();
    let project = client
        .create_project(
            &CreateProjectRequest {
                name: "Typed review".into(),
                source: ProjectSource::Repository { repo: "app".into() },
                result_mode: ResultMode::Artifacts,
            },
            "typed-project",
        )
        .unwrap();
    assert_eq!(client.project(&project.id).unwrap(), project);
    assert_eq!(client.projects().unwrap(), vec![project.clone()]);
    let operation = client
        .create_workspace(
            &CreateWorkspaceRequest {
                project_id: project.id.clone(),
                title: "Typed workspace".into(),
            },
            "typed-workspace",
        )
        .unwrap();
    assert_eq!(
        common::await_operation(&client, &operation.operation.id).state,
        branchyard_client::api::OperationState::Succeeded
    );
    // Allocation is a workspace resource, never a pretend native branch.
    // Its explicit operation binding is admitted in the same SQL transaction.
    assert!(operation.operation.branches.is_empty());
    let metadata = SqliteStore::open(fixture.data.join("state.db"), None).unwrap();
    assert!(metadata
        .console_get(
            Collection::Bindings,
            &format!("app:{}", operation.workspace_id)
        )
        .unwrap()
        .is_none());
    for path in [
        format!("/v1/operations/{}", operation.operation.id),
        "/v1/operations?idempotency_key=typed-workspace".into(),
    ] {
        let (status, _, body) = raw(server.addr, &get(&path, Some(TOKEN)));
        assert_eq!(status, 200, "{body}");
        let resolved: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(resolved["workspace_id"], operation.workspace_id);
        assert!(resolved.get("session_id").is_none());
    }
    let workspace = client.workspace(&operation.workspace_id).unwrap();
    assert_eq!(workspace.title, "Typed workspace");
    assert_eq!(client.workspaces(Some(&project.id)).unwrap().len(), 1);
    let started = client
        .start_workspace_session(
            &workspace.id,
            &StartSessionRequest {
                unapproved_tools: false,
                provider: None,
                name: "Writer".into(),
                prompt: "Hello".into(),
                harness: "gemini-cli".into(),
                model: None,
                effort: None,
                policy: None,
                provision: None,
                result_contract: None,
                access: SessionAccess::Writer,
                sources: None,
                artifacts: None,
                candidate: None,
            },
            "typed-session",
        )
        .unwrap();
    assert_eq!(
        common::await_operation(&client, &started.operation.id).state,
        branchyard_client::api::OperationState::Succeeded
    );
    let session = started.session_id.unwrap();
    for path in [
        format!("/v1/operations/{}", started.operation.id),
        "/v1/operations?idempotency_key=typed-session".into(),
    ] {
        let (status, _, body) = raw(server.addr, &get(&path, Some(TOKEN)));
        assert_eq!(status, 200, "{body}");
        let resolved: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(resolved["workspace_id"], workspace.id);
        assert_eq!(resolved["session_id"], session);
    }
    let sessions = client.workspace_sessions(&workspace.id).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, session);
    let page = client
        .workspace_messages(&workspace.id, &session, None)
        .unwrap();
    assert!(page
        .messages
        .iter()
        .any(|m| m.role == "user" && m.text == "Hello"));
    let sent = client
        .send_workspace_message(
            &workspace.id,
            &session,
            &MessageRequest {
                prompt: "Continue".into(),
                action: None,
            },
            "typed-send",
        )
        .unwrap();
    assert_eq!(sent.workspace_id, workspace.id);
    assert_eq!(
        common::await_operation(&client, &sent.operation.id).state,
        branchyard_client::api::OperationState::Succeeded
    );
}

#[test]
fn workspace_artifact_routes_reject_foreign_publishers_even_with_native_read_grants() {
    let fixture = Fixture::new();
    let server = Server::start(fixture.config());
    let client = server.client();
    run(&client, &task("WRITE own.txt=own", "own"));
    run(&client, &task("WRITE foreign.txt=foreign", "foreign"));
    let repo = client.repo("app");
    let own = repo
        .publish_artifact("own", b"own", None, None, &[], "own-artifact")
        .unwrap();
    let foreign = repo
        .publish_artifact("foreign", b"foreign", None, None, &[], "foreign-artifact")
        .unwrap();
    repo.share_artifact("foreign", &foreign.id, "own", "share-artifact")
        .unwrap();
    assert!(repo
        .artifacts("own")
        .unwrap()
        .iter()
        .any(|a| a.id == foreign.id));
    let store = SqliteStore::open(fixture.data.join("state.db"), None).unwrap();
    store
        .console_mutate(&[
            record(
                Collection::Workspaces,
                "workspace",
                json!({"repo":"app","task_id":"task"}),
            ),
            record(
                Collection::Workspaces,
                "foreign-workspace",
                json!({"repo":"app","task_id":"foreign-task"}),
            ),
            record(
                Collection::Members,
                "workspace:tester",
                json!({"workspace_id":"workspace","subject":"tester","role":"owner"}),
            ),
            record(
                Collection::Sessions,
                "different-session-id",
                json!({"workspace_id":"workspace","branch":"own"}),
            ),
            record(
                Collection::Bindings,
                "app:own",
                json!({"workspace_id":"workspace","repo":"app","branch":"own"}),
            ),
            record(
                Collection::Bindings,
                "app:foreign",
                json!({"workspace_id":"foreign-workspace","repo":"app","branch":"foreign"}),
            ),
        ])
        .unwrap();
    let (status, _, body) = raw(
        server.addr,
        &get("/v1/workspaces/workspace/artifacts", Some(TOKEN)),
    );
    assert_eq!(status, 200, "{body}");
    let listed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let artifacts = listed["artifacts"].as_array().unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0]["id"], own.id);
    assert_eq!(
        raw(
            server.addr,
            &get(
                &format!("/v1/workspaces/workspace/artifacts/{}/content", foreign.id),
                Some(TOKEN)
            )
        )
        .0,
        404
    );
    for path in [
        format!(
            "/v1/repos/app/branches/own/artifacts/{}/content",
            foreign.id
        ),
        format!("/v1/repos/app/branches/own/artifacts/{}", foreign.id),
        "/v1/repos/app/branches/own/artifacts".into(),
    ] {
        assert_eq!(raw(server.addr, &get(&path, Some(TOKEN))).0, 403, "{path}");
    }
    assert_eq!(
        raw(
            server.addr,
            &get(
                &format!("/v1/repos/app/branches/own/artifacts/{}/content", own.id),
                Some(TOKEN)
            )
        )
        .0,
        200
    );
    let binding = store
        .console_get(Collection::Bindings, "app:own")
        .unwrap()
        .unwrap();
    store
        .console_mutate(&[Mutation::Delete {
            collection: Collection::Bindings,
            id: binding.id,
            tenant: binding.tenant,
            expected: binding.revision,
        }])
        .unwrap();
    assert_eq!(
        raw(server.addr, &get("/v1/repos/app/branches/own", Some(TOKEN))).0,
        403
    );
}
