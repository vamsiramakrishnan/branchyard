//! Console metadata and native attempt integration over hermetic loopback HTTP.
//! All harness turns use the fake ACP agent; no model or cloud service is called.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report

mod common;

use branchyard_client::api::OperationState;
use branchyard_server::config::{Config, Principal, Token};
use common::{await_operation, get, post, raw, Fixture, Server, TOKEN};
use serde_json::{json, Value};

const COLLABORATOR: &str = "console-collaborator-test-token";
const OUTSIDER: &str = "console-outsider-test-token";
const RESTRICTED: &str = "console-restricted-test-token";

fn config(f: &Fixture, knowledge_only: bool) -> Config {
    let mut config = f.config();
    config.managed_knowledge.enabled = true;
    if knowledge_only {
        config.repos.clear();
    }
    for (name, secret) in [
        ("collaborator", COLLABORATOR),
        ("outsider", OUTSIDER),
        ("restricted", RESTRICTED),
    ] {
        config.tokens.push(Token {
            name: name.into(),
            secret: secret.into(),
        });
        let mut principal = Principal::default_for(name);
        if name == "outsider" {
            principal.tenant = "other-tenant".into();
        }
        if name == "restricted" {
            principal.repos = Some(["app".into()].into_iter().collect());
        }
        config.principals.insert(name.into(), principal);
    }
    config
}

fn send(server: &Server, path: &str, token: &str, key: &str, body: Value) -> (u16, Value) {
    let request = post(
        path,
        Some(token),
        &format!("Idempotency-Key: {key}\r\n"),
        &body.to_string(),
    );
    let (status, _, body) = raw(server.addr, &request);
    (status, serde_json::from_str(&body).unwrap())
}

fn read(server: &Server, path: &str, token: &str) -> (u16, Value) {
    let (status, _, body) = raw(server.addr, &get(path, Some(token)));
    (status, serde_json::from_str(&body).unwrap())
}

fn project(server: &Server, source: Value, key: &str) -> Value {
    let (status, project) = send(
        server,
        "/v1/projects",
        TOKEN,
        key,
        json!({"name":"Evidence review","source":source,"result_mode":"artifacts"}),
    );
    assert_eq!(status, 200, "{project}");
    project
}

fn workspace(server: &Server, project: &Value, key: &str) -> (String, Value) {
    let (status, operation) = send(
        server,
        "/v1/workspaces",
        TOKEN,
        key,
        json!({"project_id":project["id"],"title":"Quarterly research"}),
    );
    assert_eq!(status, 200, "{operation}");
    let finished = await_operation(&server.client(), operation["id"].as_str().unwrap());
    assert_eq!(finished.state, OperationState::Succeeded, "{finished:?}");
    let (_, collection) = read(
        server,
        &format!("/v1/workspaces?project={}", project["id"].as_str().unwrap()),
        TOKEN,
    );
    let rows = collection["workspaces"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{collection}");
    (rows[0]["id"].as_str().unwrap().into(), operation)
}

fn detail(server: &Server, workspace: &str) -> Value {
    let (status, body) = read(server, &format!("/v1/workspaces/{workspace}"), TOKEN);
    assert_eq!(status, 200, "{body}");
    body
}

#[test]
fn knowledge_project_needs_no_repository_and_replays_allocation_across_restart() {
    let f = Fixture::new();
    let config = config(&f, true);
    let server = Server::start(config.clone());
    let p = project(&server, json!({"kind":"knowledge"}), "knowledge-project");
    let repeated = project(&server, json!({"kind":"knowledge"}), "knowledge-project");
    assert_eq!(repeated, p);
    let (wid, operation) = workspace(&server, &p, "knowledge-workspace");
    let (status, replay) = send(
        &server,
        "/v1/workspaces",
        TOKEN,
        "knowledge-workspace",
        json!({"project_id":p["id"],"title":"Quarterly research"}),
    );
    assert_eq!(status, 200, "{replay}");
    assert_eq!(replay["id"], operation["id"]);
    let before = detail(&server, &wid);
    assert!(before["sessions"].as_array().unwrap().is_empty());
    assert!(before["sources"].as_array().unwrap().is_empty());
    assert!(before.to_string().find(f.dir.to_str().unwrap()).is_none());
    server.stop();
    let restarted = Server::start(config);
    assert_eq!(detail(&restarted, &wid)["task_id"], before["task_id"]);
    let (status, projects) = read(&restarted, "/v1/projects", TOKEN);
    assert_eq!(status, 200);
    assert_eq!(projects["projects"].as_array().unwrap().len(), 1);
    let (status, replay) = send(
        &restarted,
        "/v1/workspaces",
        TOKEN,
        "knowledge-workspace",
        json!({"project_id":p["id"],"title":"Quarterly research"}),
    );
    assert_eq!(status, 200, "{replay}");
    assert_eq!(replay["id"], operation["id"]);
    let (status, _) = read(&restarted, &format!("/v1/workspaces/{wid}"), OUTSIDER);
    assert!(matches!(status, 403 | 404));
    let (status, _) = send(
        &restarted,
        "/v1/projects",
        RESTRICTED,
        "restricted",
        json!({"name":"Restricted","source":{"kind":"knowledge"},"result_mode":"artifacts"}),
    );
    assert_eq!(status, 403);
    let (status, _) = send(
        &restarted,
        "/v1/projects",
        TOKEN,
        "path-input",
        json!({"name":"Bad input","source":{"kind":"knowledge","path":"/operator-private/arbitrary-host-path"},"result_mode":"artifacts"}),
    );
    assert_eq!(status, 400);
}

#[test]
fn source_originals_are_available_before_sessions_and_revision_checked() {
    let f = Fixture::new();
    let mut config = config(&f, true);
    config.managed_knowledge.max_upload_bytes = 32;
    let server = Server::start(config);
    let p = project(&server, json!({"kind":"knowledge"}), "source-project");
    let (wid, _) = workspace(&server, &p, "source-workspace");
    let before = detail(&server, &wid);
    let revision = before["revision"].as_u64().unwrap();
    let bytes = "Original evidence, unchanged.";
    let path = format!("/v1/workspaces/{wid}/sources");
    let upload = |key: &str, revision: u64, bytes: &str| {
        let request = format!("POST {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nAuthorization: Bearer {TOKEN}\r\nIdempotency-Key: {key}\r\nx-workspace-revision: {revision}\r\nx-source-title: Evidence.txt\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{bytes}", bytes.len());
        let (status, _, body) = raw(server.addr, &request);
        (status, serde_json::from_str::<Value>(&body).unwrap())
    };
    let (status, source) = upload("original", revision, bytes);
    assert_eq!(status, 200, "{source}");
    let (status, replay) = upload("original", revision, bytes);
    assert_eq!(status, 200, "{replay}");
    assert_eq!(source["id"], replay["id"]);
    assert!(source["digest"].as_str().is_some_and(|s| !s.is_empty()));
    let source_id = source["id"].as_str().unwrap();
    let download = format!("{path}/{source_id}/content");
    let (status, _, original) = raw(server.addr, &get(&download, Some(TOKEN)));
    assert_eq!(status, 200);
    assert_eq!(original, bytes);
    let (status, _) = upload("stale", revision, "second");
    assert_eq!(status, 409);
    let current = detail(&server, &wid);
    assert!(current["sessions"].as_array().unwrap().is_empty());
    assert_eq!(current["sources"].as_array().unwrap().len(), 1);
    let (status, _) = upload(
        "oversize",
        current["revision"].as_u64().unwrap(),
        &"x".repeat(33),
    );
    assert!(matches!(status, 400 | 413));
    let (status, _, _) = raw(server.addr, &get(&download, Some(OUTSIDER)));
    assert!(matches!(status, 403 | 404));
    let (status, rejected) = send(
        &server,
        &format!("/v1/workspaces/{wid}/finalize"),
        TOKEN,
        "invalid-acceptance",
        json!({"revision":current["revision"],"artifacts":[source_id]}),
    );
    assert!(matches!(status, 400 | 404 | 409), "{rejected}");
    let (status, unavailable) = send(
        &server,
        &format!("/v1/workspaces/{wid}/actions/sleep"),
        TOKEN,
        "sleep-unavailable",
        json!({"revision":current["revision"]}),
    );
    assert_eq!(status, 422, "{unavailable}");
    let (status, archive) = send(
        &server,
        &format!("/v1/workspaces/{wid}/actions/archive"),
        TOKEN,
        "archive",
        json!({"revision":current["revision"]}),
    );
    assert_eq!(status, 200, "{archive}");
    assert_eq!(
        await_operation(&server.client(), archive["id"].as_str().unwrap()).state,
        OperationState::Succeeded
    );
    let archived = detail(&server, &wid);
    assert_eq!(archived["state"], "archived");
    assert_eq!(archived["sources"].as_array().unwrap().len(), 1);
    assert_eq!(raw(server.addr, &get(&download, Some(TOKEN))).2, bytes);
    let (status, restore) = send(
        &server,
        &format!("/v1/workspaces/{wid}/actions/restore"),
        TOKEN,
        "restore",
        json!({"revision":archived["revision"]}),
    );
    assert_eq!(status, 200, "{restore}");
    assert_eq!(
        await_operation(&server.client(), restore["id"].as_str().unwrap()).state,
        OperationState::Succeeded
    );
    assert_eq!(detail(&server, &wid)["state"], "ready");
    let (status, start) = send(
        &server,
        &format!("/v1/workspaces/{wid}/sessions"),
        TOKEN,
        "read-selected-source",
        json!({"name":"Source reader","prompt":"SH cat .console-inputs/*/*/*","harness":"gemini-cli","access":"writer","sources":[source_id]}),
    );
    assert_eq!(status, 200, "{start}");
    assert_eq!(
        await_operation(&server.client(), start["id"].as_str().unwrap()).state,
        OperationState::Succeeded
    );
    let sid = start["session_id"].as_str().unwrap();
    let (status, messages) = read(
        &server,
        &format!("/v1/workspaces/{wid}/sessions/{sid}/messages"),
        TOKEN,
    );
    assert_eq!(status, 200, "{messages}");
    let assistant_text: String = messages["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "assistant")
        .map(|message| message["text"].as_str().unwrap())
        .collect();
    assert!(assistant_text.contains(bytes), "{messages}");
}

#[test]
fn comments_are_attributed_and_removed_members_lose_legacy_operation_and_task_access() {
    let f = Fixture::new();
    let server = Server::start(config(&f, false));
    let p = project(
        &server,
        json!({"kind":"repository","repo":"app"}),
        "collaboration-project",
    );
    let (wid, operation) = workspace(&server, &p, "collaboration-workspace");
    let members_path = format!("/v1/workspaces/{wid}/members/collaborator");
    let revision = detail(&server, &wid)["revision"].clone();
    let (status, member) = send(
        &server,
        &members_path,
        TOKEN,
        "add-collaborator",
        json!({"revision":revision,"role":"reviewer"}),
    );
    assert_eq!(status, 200, "{member}");
    let (status, _) = read(&server, &format!("/v1/workspaces/{wid}"), COLLABORATOR);
    assert_eq!(status, 200);
    let revision = detail(&server, &wid)["revision"].clone();
    let comment_path = format!("/v1/workspaces/{wid}/comments");
    let comment_request =
        json!({"revision":revision,"text":"Check the source citations.","subject":"forged-author"});
    let (status, comment) = send(
        &server,
        &comment_path,
        COLLABORATOR,
        "review-comment",
        comment_request.clone(),
    );
    assert_eq!(status, 200, "{comment}");
    assert_eq!(comment["subject"], "collaborator");
    assert_eq!(comment["credential"], "collaborator");
    let (status, replay) = send(
        &server,
        &comment_path,
        COLLABORATOR,
        "review-comment",
        comment_request,
    );
    assert_eq!(status, 200, "{replay}");
    assert_eq!(replay["id"], comment["id"]);
    let (status, _) = send(
        &server,
        &comment_path,
        COLLABORATOR,
        "stale-review",
        json!({"revision":revision,"text":"stale"}),
    );
    assert_eq!(status, 409);
    let (status, _) = send(
        &server,
        &members_path,
        COLLABORATOR,
        "escalate",
        json!({"revision":detail(&server, &wid)["revision"],"role":"owner"}),
    );
    assert_eq!(status, 403);
    let (status, presence) = send(
        &server,
        &format!("/v1/workspaces/{wid}/presence"),
        COLLABORATOR,
        "presence",
        json!({"ttl_seconds":30,"subject":"forged-author"}),
    );
    assert_eq!(status, 200, "{presence}");
    assert_eq!(presence["subject"], "collaborator");
    assert!(presence["expires_at_ms"].as_u64().unwrap() > branchyard_support::time::now_ms());
    let operation_path = format!("/v1/operations/{}", operation["id"].as_str().unwrap());
    assert_eq!(read(&server, &operation_path, COLLABORATOR).0, 200);
    let revision = detail(&server, &wid)["revision"].clone();
    let body = json!({"revision":revision}).to_string();
    let request = post(
        &members_path,
        Some(TOKEN),
        "Idempotency-Key: revoke\r\n",
        &body,
    )
    .replacen("POST ", "DELETE ", 1);
    assert_eq!(raw(server.addr, &request).0, 200);
    let task_id = detail(&server, &wid)["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    for path in [
        format!("/v1/workspaces/{wid}"),
        comment_path,
        operation_path,
        format!("/v1/repos/app/task-records/{task_id}"),
    ] {
        let (status, value) = read(&server, &path, COLLABORATOR);
        assert!(matches!(status, 403 | 404), "{path}: {status} {value}");
    }
    let revision = detail(&server, &wid)["revision"].clone();
    let (status, _) = send(
        &server,
        &format!("/v1/workspaces/{wid}/members/tester"),
        TOKEN,
        "last-owner",
        json!({"revision":revision,"role":"viewer"}),
    );
    assert_eq!(status, 409);
}

#[test]
fn named_writer_sessions_keep_separate_attempts_and_native_conversations() {
    let f = Fixture::new();
    let server = Server::start(config(&f, true));
    let p = project(&server, json!({"kind":"knowledge"}), "sessions-project");
    let (wid, _) = workspace(&server, &p, "sessions-workspace");
    let sessions_path = format!("/v1/workspaces/{wid}/sessions");
    let start = |name: &str, prompt: &str, key: &str| {
        let (status, operation) = send(
            &server,
            &sessions_path,
            TOKEN,
            key,
            json!({"name":name,"prompt":prompt,"harness":"gemini-cli","access":"writer"}),
        );
        assert_eq!(status, 200, "{operation}");
        let finished = await_operation(&server.client(), operation["id"].as_str().unwrap());
        assert_eq!(finished.state, OperationState::Succeeded, "{finished:?}");
        operation
    };
    let first = start("Researcher", "WHOAMI", "researcher");
    let second = start("Writer", "A distinct writer conversation", "writer");
    let first_id = first["session_id"].as_str().unwrap();
    let second_id = second["session_id"].as_str().unwrap();
    assert_ne!(first_id, second_id);
    let (status, sessions) = read(&server, &sessions_path, TOKEN);
    assert_eq!(status, 200, "{sessions}");
    assert_eq!(sessions["sessions"].as_array().unwrap().len(), 2);
    let first_messages = format!("{sessions_path}/{first_id}/messages");
    let second_messages = format!("{sessions_path}/{second_id}/messages");
    let (status, transcript) = read(&server, &first_messages, TOKEN);
    assert_eq!(status, 200, "{transcript}");
    assert!(transcript["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "user" && m["text"] == "WHOAMI"));
    assert!(!transcript
        .to_string()
        .contains("A distinct writer conversation"));
    let (_, other) = read(&server, &second_messages, TOKEN);
    assert!(other.to_string().contains("A distinct writer conversation"));
    let (status, continued) = send(
        &server,
        &first_messages,
        TOKEN,
        "continue-researcher",
        json!({"prompt":"Follow up in the research conversation","action":"resume"}),
    );
    assert_eq!(status, 200, "{continued}");
    let finished = await_operation(&server.client(), continued["id"].as_str().unwrap());
    assert_eq!(finished.state, OperationState::Succeeded, "{finished:?}");
    let (_, transcript) = read(&server, &first_messages, TOKEN);
    assert!(transcript
        .to_string()
        .contains("Follow up in the research conversation"));
    let (_, other) = read(&server, &second_messages, TOKEN);
    assert!(!other
        .to_string()
        .contains("Follow up in the research conversation"));
    let (status, _) = send(
        &server,
        &sessions_path,
        TOKEN,
        "review-without-handoff",
        json!({"name":"Reviewer","prompt":"Review","harness":"gemini-cli","access":"reviewer"}),
    );
    assert_eq!(status, 400);
}

#[test]
fn workspace_quota_serializes_allocations_across_projects() {
    use std::sync::{Arc, Barrier};
    let f = Fixture::new();
    let mut config = config(&f, true);
    config.managed_knowledge.max_workspaces = 1;
    let server = Server::start(config);
    let left = project(&server, json!({"kind":"knowledge"}), "quota-left-project");
    let right = project(&server, json!({"kind":"knowledge"}), "quota-right-project");
    let addr = server.addr;
    let barrier = Arc::new(Barrier::new(2));
    let submit = |project: Value, key: &str, barrier: Arc<Barrier>| {
        let body = json!({"project_id":project["id"],"title":"Quota reservation"});
        let request = post(
            "/v1/workspaces",
            Some(TOKEN),
            &format!("Idempotency-Key: {key}\r\n"),
            &body.to_string(),
        );
        barrier.wait();
        let (status, _, body) = raw(addr, &request);
        (status, serde_json::from_str::<Value>(&body).unwrap())
    };
    let (first, second) = std::thread::scope(|scope| {
        let other = Arc::clone(&barrier);
        let first = scope.spawn(|| submit(left, "quota-left", other));
        let second = submit(right.clone(), "quota-right", barrier);
        (first.join().unwrap(), second)
    });
    assert_eq!(
        [first.0, second.0]
            .into_iter()
            .filter(|status| *status == 200)
            .count(),
        1,
        "{first:?} {second:?}"
    );
    let refused = if first.0 == 200 { &second } else { &first };
    assert!(matches!(refused.0, 409 | 429), "{refused:?}");
    let accepted = if first.0 == 200 { &first } else { &second };
    let operation = await_operation(&server.client(), accepted.1["id"].as_str().unwrap());
    assert_eq!(operation.state, OperationState::Succeeded, "{operation:?}");
    let (status, all) = read(&server, "/v1/workspaces", TOKEN);
    assert_eq!(status, 200);
    assert_eq!(all["workspaces"].as_array().unwrap().len(), 1);
    let (status, denied) = send(
        &server,
        "/v1/workspaces",
        TOKEN,
        "quota-after-race",
        json!({"project_id":right["id"],"title":"Over quota"}),
    );
    assert_eq!(status, 429, "{denied}");
}

#[test]
fn queued_session_rechecks_membership_before_running_a_turn() {
    let f = Fixture::new();
    let mut config = config(&f, false);
    config.max_running = 1;
    let server = Server::start(config);
    let p = project(
        &server,
        json!({"kind":"repository","repo":"app"}),
        "revocation-project",
    );
    let (wid, _) = workspace(&server, &p, "revocation-workspace");
    let members_path = format!("/v1/workspaces/{wid}/members/collaborator");
    let (status, member) = send(
        &server,
        &members_path,
        TOKEN,
        "admit-editor",
        json!({"revision":detail(&server, &wid)["revision"],"role":"editor"}),
    );
    assert_eq!(status, 200, "{member}");
    let client = server.client();
    let blocker = client
        .repo("app")
        .submit_task(
            &common::task("HANG", "queue-blocker"),
            &branchyard_client::new_key(),
        )
        .unwrap();
    branchyard_testkit::wait::until("the fake blocking turn to receive its prompt", || {
        common::started(&client, "app", "queue-blocker")
    });
    let (status, queued) = send(
        &server,
        &format!("/v1/workspaces/{wid}/sessions"),
        COLLABORATOR,
        "queued-writer",
        json!({"name":"Revoked writer","prompt":"This turn must never start","harness":"gemini-cli","access":"writer"}),
    );
    assert_eq!(status, 200, "{queued}");
    let queued_id = queued["id"].as_str().unwrap();
    assert_eq!(
        client.operation(queued_id).unwrap().state,
        OperationState::Queued
    );
    let (status, busy) = send(
        &server,
        &format!("/v1/workspaces/{wid}/actions/archive"),
        TOKEN,
        "archive-queued-work",
        json!({"revision":detail(&server, &wid)["revision"]}),
    );
    assert_eq!(status, 409, "{busy}");
    assert_eq!(detail(&server, &wid)["state"], "ready");
    let body = json!({"revision":detail(&server, &wid)["revision"]}).to_string();
    let request = post(
        &members_path,
        Some(TOKEN),
        "Idempotency-Key: revoke-before-execution\r\n",
        &body,
    )
    .replacen("POST ", "DELETE ", 1);
    assert_eq!(raw(server.addr, &request).0, 200);
    client.repo("app").cancel("queue-blocker").unwrap();
    await_operation(&client, &blocker.id);
    let rejected = await_operation(&client, queued_id);
    assert_eq!(rejected.state, OperationState::Failed, "{rejected:?}");
    assert_eq!(
        rejected.error.as_ref().unwrap().code,
        "workspace_access_denied"
    );
    let native = branchyard::Yard::open(&f.root).unwrap();
    let session_id = queued["session_id"].as_str().unwrap();
    assert!(matches!(
        native.branch(session_id),
        Err(branchyard::Error::UnknownBranch(_))
    ));
    let task_id = detail(&server, &wid)["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!branchyard::tasks::view(&native, &task_id)
        .unwrap()
        .attempts
        .iter()
        .any(|attempt| attempt.name == session_id));
    let (status, _) = read(
        &server,
        &format!("/v1/operations/{queued_id}"),
        COLLABORATOR,
    );
    assert!(matches!(status, 403 | 404));
}

#[test]
fn knowledge_artifacts_can_be_accepted_without_a_git_diff_and_stay_turn_bound() {
    let f = Fixture::new();
    let server = Server::start(config(&f, true));
    let p = project(&server, json!({"kind":"knowledge"}), "artifact-project");
    let (wid, _) = workspace(&server, &p, "artifact-workspace");
    let mut contract = branchyard::ResultContract::deliverable(json!({
        "type":"object","required":["answer"],"additionalProperties":false,
        "properties":{"answer":{"type":"string"}}
    }))
    .unwrap();
    contract.capture = Some(branchyard::ResultCapture::new(
        ".cache/outputs/artifacts.json",
    ));
    // The task's existing default ignores .cache/. Capture is explicit and
    // independent of Git, as in the core artifact-capture regressions.
    let prompt = concat!(
        "SH mkdir -p .cache/outputs && printf %s 'checked evidence' > .cache/outputs/evidence.txt && ",
        "printf %s '{\"schema_version\":1,\"files\":[{\"path\":\"evidence.txt\",\"media_type\":\"text/plain\",\"sha256\":\"a06580d6ceda176885b7d46ba52084b1743497efd00763c5491c4fbeea45efe3\"}]}' > .cache/outputs/artifacts.json && ",
        "printf '\\n```json\\n{\"answer\":\"checked\"}\\n```\\n'"
    );
    let (status, started) = send(
        &server,
        &format!("/v1/workspaces/{wid}/sessions"),
        TOKEN,
        "produce-artifact",
        json!({"name":"Evidence writer","prompt":prompt,"harness":"gemini-cli","access":"writer","result_contract":contract}),
    );
    assert_eq!(status, 200, "{started}");
    let finished = await_operation(&server.client(), started["id"].as_str().unwrap());
    assert_eq!(finished.state, OperationState::Succeeded, "{finished:?}");
    let workspace = detail(&server, &wid);
    let session = &workspace["sessions"][0];
    assert_eq!(session["status"], "no_changes", "{session}");
    let (status, artifacts) = read(&server, &format!("/v1/workspaces/{wid}/artifacts"), TOKEN);
    assert_eq!(status, 200, "{artifacts}");
    assert_eq!(artifacts["artifacts"].as_array().unwrap().len(), 1);
    let artifact = artifacts["artifacts"][0]["id"].as_str().unwrap();
    let finalize_path = format!("/v1/workspaces/{wid}/finalize");
    let accepted_request = json!({"revision":workspace["revision"],"artifacts":[artifact]});
    let (status, accepted) = send(
        &server,
        &finalize_path,
        TOKEN,
        "accept-evidence",
        accepted_request.clone(),
    );
    assert_eq!(status, 200, "{accepted}");
    assert_eq!(accepted["artifacts"], json!([artifact]));
    assert_eq!(accepted["code_integration"], "independent");
    let (status, replay) = send(
        &server,
        &finalize_path,
        TOKEN,
        "accept-evidence",
        accepted_request.clone(),
    );
    assert_eq!(status, 200, "{replay}");
    assert_eq!(replay, accepted);
    let (status, _) = send(
        &server,
        &finalize_path,
        TOKEN,
        "stale-acceptance",
        accepted_request,
    );
    assert_eq!(status, 409);
    let current = detail(&server, &wid);
    assert_eq!(
        current["accepted_deliverables"].as_array().unwrap().len(),
        1
    );
    let (status, _, bytes) = raw(
        server.addr,
        &get(
            &format!("/v1/workspaces/{wid}/artifacts/{artifact}/content"),
            Some(TOKEN),
        ),
    );
    assert_eq!(status, 200);
    assert_eq!(bytes, "checked evidence");
    let sid = started["session_id"].as_str().unwrap();
    let (status, revised) = send(
        &server,
        &format!("/v1/workspaces/{wid}/sessions/{sid}/messages"),
        TOKEN,
        "revise-evidence",
        json!({"prompt":"SH printf '\\n```json\\n{\"answer\":\"revised\"}\\n```\\n'"}),
    );
    assert_eq!(status, 200, "{revised}");
    assert_eq!(
        await_operation(&server.client(), revised["id"].as_str().unwrap()).state,
        OperationState::Succeeded
    );
    let (status, stale) = send(
        &server,
        &finalize_path,
        TOKEN,
        "previous-turn-artifact",
        json!({"revision":detail(&server, &wid)["revision"],"artifacts":[artifact]}),
    );
    assert_eq!(status, 409, "{stale}");
}

#[test]
fn session_branch_quota_counts_completed_attempts_in_other_managed_yards() {
    use branchyard_server::config::{TenantPolicy, DEFAULT_TENANT};
    let f = Fixture::new();
    let mut config = config(&f, true);
    config.tenants.insert(
        DEFAULT_TENANT.into(),
        TenantPolicy {
            max_branches: Some(1),
            ..Default::default()
        },
    );
    let server = Server::start(config);
    let first_project = project(
        &server,
        json!({"kind":"knowledge"}),
        "branch-quota-project-one",
    );
    let (first_workspace, _) = workspace(&server, &first_project, "branch-quota-workspace-one");
    let request =
        json!({"name":"Writer","prompt":"WHOAMI","harness":"gemini-cli","access":"writer"});
    let (status, operation) = send(
        &server,
        &format!("/v1/workspaces/{first_workspace}/sessions"),
        TOKEN,
        "branch-quota-first",
        request.clone(),
    );
    assert_eq!(status, 200, "{operation}");
    let finished = await_operation(&server.client(), operation["id"].as_str().unwrap());
    assert_eq!(finished.state, OperationState::Succeeded, "{finished:?}");
    // Creating another workspace must preserve the tenant admission counter.
    let second_project = project(
        &server,
        json!({"kind":"knowledge"}),
        "branch-quota-project-two",
    );
    let (second_workspace, _) = workspace(&server, &second_project, "branch-quota-workspace-two");
    let (status, denied) = send(
        &server,
        &format!("/v1/workspaces/{second_workspace}/sessions"),
        TOKEN,
        "branch-quota-second",
        request,
    );
    assert_eq!(status, 429, "{denied}");
    assert_eq!(denied["error"]["detail"]["limit"], "max_branches");
    assert!(detail(&server, &second_workspace)["sessions"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn console_starts_apply_native_cost_and_artifact_ceilings() {
    use branchyard_server::config::{TenantPolicy, DEFAULT_TENANT};
    for limit in ["max_cost_usd", "max_artifact_bytes"] {
        let f = Fixture::new();
        let mut config = config(&f, true);
        let mut policy = TenantPolicy::default();
        if limit == "max_cost_usd" {
            policy.max_cost_usd = Some(0.0);
        } else {
            policy.max_artifact_bytes = Some(0);
        }
        config.tenants.insert(DEFAULT_TENANT.into(), policy);
        let server = Server::start(config);
        let project = project(&server, json!({"kind":"knowledge"}), "usage-project");
        let (wid, _) = workspace(&server, &project, "usage-workspace");
        let (status, denied) = send(
            &server,
            &format!("/v1/workspaces/{wid}/sessions"),
            TOKEN,
            "usage-start",
            json!({"name":"Writer","prompt":"WHOAMI","harness":"gemini-cli","access":"writer"}),
        );
        assert_eq!(status, 429, "{denied}");
        assert_eq!(denied["error"]["detail"]["limit"], limit);
        assert!(detail(&server, &wid)["sessions"]
            .as_array()
            .unwrap()
            .is_empty());
    }
}

#[test]
fn queued_cancel_runs_when_global_and_tenant_turn_capacity_are_full() {
    use branchyard_server::config::{TenantPolicy, DEFAULT_TENANT};
    let f = Fixture::new();
    let mut config = config(&f, true);
    config.max_running = 1;
    config.tenants.insert(
        DEFAULT_TENANT.into(),
        TenantPolicy {
            max_running: Some(1),
            ..Default::default()
        },
    );
    let server = Server::start(config);
    let project = project(&server, json!({"kind":"knowledge"}), "saturated-project");
    let (wid, _) = workspace(&server, &project, "saturated-workspace");
    let sessions = format!("/v1/workspaces/{wid}/sessions");
    let (status, held) = send(
        &server,
        &sessions,
        TOKEN,
        "saturated-start",
        json!({"name":"Held writer","prompt":"HANG","harness":"gemini-cli","access":"writer"}),
    );
    assert_eq!(status, 200, "{held}");
    let sid = held["session_id"].as_str().unwrap();
    let messages = format!("{sessions}/{sid}/messages");
    branchyard_testkit::wait::until("the fake held turn to record its prompt", || {
        let (status, value) = read(&server, &messages, TOKEN);
        status == 200
            && value["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("HANG"))
                })
            })
    });
    let (status, cancel) = send(
        &server,
        &format!("{sessions}/{sid}/cancel"),
        TOKEN,
        "saturated-cancel",
        json!({}),
    );
    assert_eq!(status, 200, "{cancel}");
    let cancelled = await_operation(&server.client(), cancel["id"].as_str().unwrap());
    assert_eq!(cancelled.state, OperationState::Succeeded, "{cancelled:?}");
    let finished = await_operation(&server.client(), held["id"].as_str().unwrap());
    assert!(finished.state.is_terminal(), "{finished:?}");
    let (status, sessions) = read(&server, &sessions, TOKEN);
    assert_eq!(status, 200, "{sessions}");
    assert_eq!(sessions["sessions"][0]["status"], "interrupted");
}

#[test]
fn reads_never_initialize_a_failed_workspace_and_queued_restore_resumes_its_reservation() {
    let f = Fixture::new();
    let mut config = config(&f, false);
    config.max_running = 1;
    let managed = f.dir.join("managed-knowledge");
    config.managed_knowledge.root = Some(managed.clone());
    std::fs::write(&managed, "initialization is deliberately unavailable").unwrap();
    let server = Server::start(config);
    let p = project(&server, json!({"kind":"knowledge"}), "deferred-project");
    let (status, initialize) = send(
        &server,
        "/v1/workspaces",
        TOKEN,
        "deferred-workspace",
        json!({"project_id":p["id"],"title":"Resumable allocation"}),
    );
    assert_eq!(status, 200, "{initialize}");
    let initial = await_operation(&server.client(), initialize["id"].as_str().unwrap());
    assert_eq!(initial.state, OperationState::Failed, "{initial:?}");
    let wid = initialize["workspace_id"].as_str().unwrap();
    let pending = detail(&server, wid);
    let task_id = pending["task_id"].as_str().unwrap();
    std::fs::remove_file(&managed).unwrap();
    let artifacts_path = format!("/v1/workspaces/{wid}/artifacts");
    let (status, unavailable) = read(&server, &artifacts_path, TOKEN);
    assert_eq!(status, 409, "{unavailable}");
    assert!(!managed.exists());
    let client = server.client();
    let blocker = client
        .repo("app")
        .submit_task(
            &common::task("HANG", "restore-blocker"),
            &branchyard_client::new_key(),
        )
        .unwrap();
    branchyard_testkit::wait::until("the fake restore blocker to receive its prompt", || {
        common::started(&client, "app", "restore-blocker")
    });
    let (status, restore) = send(
        &server,
        &format!("/v1/workspaces/{wid}/actions/restore"),
        TOKEN,
        "resume-allocation",
        json!({"revision":pending["revision"]}),
    );
    assert_eq!(status, 200, "{restore}");
    assert_eq!(
        client
            .operation(restore["id"].as_str().unwrap())
            .unwrap()
            .state,
        OperationState::Queued
    );
    assert!(
        !managed.exists(),
        "restore admission must not initialize Git"
    );
    assert_eq!(read(&server, &artifacts_path, TOKEN).0, 409);
    assert!(!managed.exists(), "a GET must not initialize Git");
    client.repo("app").cancel("restore-blocker").unwrap();
    await_operation(&client, &blocker.id);
    let restored = await_operation(&client, restore["id"].as_str().unwrap());
    assert_eq!(restored.state, OperationState::Succeeded, "{restored:?}");
    let ready = detail(&server, wid);
    assert_eq!(ready["task_id"], task_id);
    assert_eq!(ready["state"], "ready");
    assert!(managed.join("tasks").join(task_id).join("work").is_dir());
    assert_eq!(std::fs::read_dir(managed.join("tasks")).unwrap().count(), 1);
    assert_eq!(read(&server, &artifacts_path, TOKEN).0, 200);
}

#[test]
fn console_turns_keep_registered_sync_leases_and_refuse_unmapped_managed_sources() {
    use branchyard_sync::engine::{Options, Remote};
    use branchyard_sync::store::file::FileStore;
    use std::sync::Arc;
    use std::time::Duration;

    let f = Fixture::new();
    let bucket = f.dir.join("console-sync-bucket");
    let mut config = config(&f, false);
    config.sync = Some(branchyard_server::config::SyncSettings {
        config: branchyard_sync::SyncConfig {
            remote: format!("file://{}", bucket.display()),
            interval: Some("1s".into()),
            device: Some("console-server".into()),
            ..Default::default()
        },
        lease: Duration::from_secs(30),
    });
    let server = Server::start(config);
    let p = project(
        &server,
        json!({"kind":"repository","repo":"app"}),
        "synced-project",
    );
    let (wid, _) = workspace(&server, &p, "synced-workspace");
    let (status, start) = send(
        &server,
        &format!("/v1/workspaces/{wid}/sessions"),
        TOKEN,
        "synced-writer",
        json!({"name":"Synced writer","prompt":"WHOAMI","harness":"gemini-cli","access":"writer"}),
    );
    assert_eq!(status, 200, "{start}");
    assert_eq!(
        await_operation(&server.client(), start["id"].as_str().unwrap()).state,
        OperationState::Succeeded
    );
    let sid = start["session_id"].as_str().unwrap();
    let mut options = Options::default();
    options.settings.device = "other-runner".into();
    let remote = Remote::open(Arc::new(FileStore::open(&bucket).unwrap()), options).unwrap();
    let task_id = branchyard_testkit::wait::until(
        "the console native attempt to reach the file sync remote",
        || {
            remote.tasks().ok().and_then(|tasks| {
                tasks
                    .into_iter()
                    .find(|task| task.task.ends_with(&format!(".{sid}")))
                    .map(|task| task.task)
            })
        },
    );
    let held = remote
        .acquire_lease(&task_id, "run", "other-runner", Duration::from_secs(300))
        .unwrap();
    let path = format!("/v1/workspaces/{wid}/sessions/{sid}/messages");
    let (status, denied) = send(
        &server,
        &path,
        TOKEN,
        "lease-held-send",
        json!({"prompt":"This continuation must not run"}),
    );
    assert_eq!(status, 200, "{denied}");
    let refused = await_operation(&server.client(), denied["id"].as_str().unwrap());
    assert_eq!(refused.state, OperationState::Failed, "{refused:?}");
    assert_eq!(refused.error.as_ref().unwrap().code, "sync_lease_held");
    assert!(!read(&server, &path, TOKEN)
        .1
        .to_string()
        .contains("This continuation must not run"));
    remote.release_lease(held).unwrap();
    let (status, accepted) = send(
        &server,
        &path,
        TOKEN,
        "lease-released-send",
        json!({"prompt":"A continuation under the renewed native lease"}),
    );
    assert_eq!(status, 200, "{accepted}");
    assert_eq!(
        await_operation(&server.client(), accepted["id"].as_str().unwrap()).state,
        OperationState::Succeeded
    );
    let p = project(&server, json!({"kind":"knowledge"}), "unmapped-project");
    let (wid, _) = workspace(&server, &p, "unmapped-workspace");
    let (status, start) = send(
        &server,
        &format!("/v1/workspaces/{wid}/sessions"),
        TOKEN,
        "unmapped-writer",
        json!({"name":"Unmapped writer","prompt":"This managed turn must not run","harness":"gemini-cli","access":"writer"}),
    );
    assert_eq!(status, 200, "{start}");
    let refused = await_operation(&server.client(), start["id"].as_str().unwrap());
    assert_eq!(refused.state, OperationState::Failed, "{refused:?}");
    assert_eq!(
        refused.error.as_ref().unwrap().code,
        "sync_source_unsupported"
    );
    let path = format!(
        "/v1/workspaces/{wid}/sessions/{}/messages",
        start["session_id"].as_str().unwrap()
    );
    assert_eq!(read(&server, &path, TOKEN).0, 404);
}
