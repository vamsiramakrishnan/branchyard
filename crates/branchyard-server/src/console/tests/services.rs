//! Actual authenticated server routes and SQLite queue, without injected gateways.
#![cfg(unix)]

use super::*;
use crate::console_pr_gateway::PrCommand;
use crate::store::SqliteStore;
use branchyard_client::api::OperationState;
use std::os::unix::fs::PermissionsExt;

const PUBLISHER: &str = "hermetic-merge-only-token";

/// Operator configuration and repository are installed before any Weak<App> exists.
struct Case {
    fixture: Fixture,
    _repo: branchyard_testkit::Repo,
}

impl Case {
    fn new(body: &str, terminal: bool) -> Self {
        Self::with_ack(body, terminal, false)
    }

    fn with_ack(body: &str, terminal: bool, unapproved: bool) -> Self {
        let mut fixture = Fixture::new(false);
        let repo = branchyard_testkit::Repo::init("unused-by");
        let app = Arc::get_mut(&mut fixture.app).unwrap();
        app.config.allow_client_commands = terminal;
        app.config.allow_unapproved_tools = unapproved;
        app.config.tokens.push(Token {
            name: "publisher".into(),
            secret: PUBLISHER.into(),
        });
        let mut publisher = Principal::default_for("owner");
        publisher.scopes = ["read".into(), "merge".into()].into_iter().collect();
        app.config.principals.insert("publisher".into(), publisher);
        app.credentials = Credentials::new(app.config.all_credentials());
        app.config.allow_providers.insert("microsandbox".into());
        app.config.harness_commands.insert(
            "codex-acp".into(),
            vec![branchyard_testkit::fake_agent_here().display().to_string()],
        );
        let agy = app.config.data_dir.parent().unwrap().join("fake-agy");
        std::fs::write(&agy,r#"#!/usr/bin/python3
import json, sys
session = sys.argv[sys.argv.index('--conversation') + 1] if '--conversation' in sys.argv else 'hermetic-agy-session'
print(json.dumps({'event':'init','conversation_id':session,'init':{'cwd':'.','tools':[]}}),flush=True)
for number, line in enumerate(sys.stdin):
    json.loads(line)
    print(json.dumps({'event':'step_update','step_update':{'conversation_id':session,'step_index':number,'step_type':'user_input','state':'DONE'}}),flush=True)
    print(json.dumps({'event':'result','result':{'conversation_id':session,'status':'SUCCESS','response':'Checked evidence','num_turns':number+1}}),flush=True)
"#).unwrap();
        std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o755)).unwrap();
        app.config
            .harness_commands
            .insert("antigravity".into(), vec![agy.display().to_string()]);
        let by = app.config.data_dir.parent().unwrap().join("fake-by");
        std::fs::write(
            &by,
            format!("#!/bin/sh\nprintf called >> forge-calls\n{body}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&by, std::fs::Permissions::from_mode(0o755)).unwrap();
        app.config.by_path = Some(by);
        app.config.repos.push(("code".into(), repo.root.clone()));
        let yard = crate::serve::open_yard(&app.config, "code", &repo.root).unwrap();
        app.repos.insert(
            "code".into(),
            crate::api::RepoState {
                name: "code".into(),
                feed: Arc::new(crate::feed::Feed::open(yard.clone()).unwrap()),
                yard,
                wake: Arc::new(tokio::sync::Notify::new()),
            },
        );
        let mut options = Options::new(vec!["code".into(), super::super::MANAGED_REPO.into()]);
        options.poll = Duration::from_millis(10);
        let store = SqliteStore::open(
            app.config
                .data_dir
                .parent()
                .unwrap()
                .join("service-operations.db"),
            None,
        )
        .unwrap();
        app.registry = Registry::open(Box::new(store), options).unwrap();
        fixture.start();
        Self {
            fixture,
            _repo: repo,
        }
    }

    async fn writer(&self) -> (String, String) {
        self.writer_on(None).await
    }

    async fn writer_on(&self, provider: Option<branchyard::Provider>) -> (String, String) {
        let f = &self.fixture;
        let (status,p)=f.json(Method::POST,"/v1/projects",OWNER,Some("code-project"),json!({"name":"Code","source":{"kind":"repository","repo":"code"},"result_mode":"mixed"})).await;
        assert_eq!(status, StatusCode::OK, "{p}");
        let (status, w) = f
            .json(
                Method::POST,
                "/v1/workspaces",
                OWNER,
                Some("code-workspace"),
                json!({"project_id":p["id"],"title":"Review changes"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{w}");
        assert_eq!(
            f.finished(w["id"].as_str().unwrap()).state,
            OperationState::Succeeded
        );
        let wid = w["workspace_id"].as_str().unwrap().to_owned();
        let (status,s)=f.json(Method::POST,&format!("/v1/workspaces/{wid}/sessions"),OWNER,Some("writer"),json!({"name":"Writer","prompt":"WRITE report.txt=checked","harness":"codex-acp","access":"writer","provider":provider})).await;
        assert_eq!(status, StatusCode::OK, "{s}");
        let done = f.finished(s["id"].as_str().unwrap());
        assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
        (wid, s["session_id"].as_str().unwrap().into())
    }

    fn row(&self, wid: &str) -> crate::console_store::Record {
        self.fixture
            .app
            .registry
            .console_get(Collection::Members, &format!("{wid}:owner"))
            .unwrap()
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publication_requires_merge_scope_without_requiring_run() {
    let case = Case::new("printf '%s' '{\"number\":42}'", false);
    let (wid, sid) = case.writer().await;
    let f = &case.fixture;
    let path = format!("/v1/workspaces/{wid}/sessions/{sid}");
    let (status, op) = f
        .json(
            Method::POST,
            &format!("{path}/pr"),
            PUBLISHER,
            Some("merge-only"),
            json!({"title":"Publish reviewed evidence"}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{op}");
    assert_eq!(op["kind"], "merge");
    let done = f.finished(op["id"].as_str().unwrap());
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    assert_eq!(done.result.unwrap().pull_request.unwrap()["number"], 42);
    let (status, error) = f
        .json(
            Method::POST,
            &format!("{path}/runtime/actions/pause"),
            PUBLISHER,
            Some("merge-cannot-run"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["error"]["code"], "scope_required");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mounted_pr_jobs_keep_results_and_uncertain_replays_durably() {
    let case=Case::new("printf '%s' '{\"comments\":[{\"body\":\"Check sources\"}],\"reviews\":[{\"state\":\"APPROVED\"}]}'",false);
    let (wid, sid) = case.writer().await;
    let f = &case.fixture;
    let path = format!("/v1/workspaces/{wid}/sessions/{sid}/pr");
    let (status, op) = f
        .json(
            Method::POST,
            &path,
            OWNER,
            Some("publish-once"),
            json!({"title":"$(touch injected)"}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{op}");
    let first = f.finished(op["id"].as_str().unwrap());
    assert_eq!(first.state, OperationState::Succeeded, "{first:?}");
    assert_eq!(
        first.result.unwrap().pull_request.unwrap()["reviews"][0]["state"],
        "APPROVED"
    );
    let (_, retrieved) = f
        .json(
            Method::GET,
            &format!("/v1/operations/{}", op["id"].as_str().unwrap()),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(
        retrieved["result"]["pull_request"]["comments"][0]["body"],
        "Check sources"
    );
    assert!(!case._repo.root.join("injected").exists());
    let by = f.app.config.by_path.as_ref().unwrap();
    std::fs::write(
        by,
        "#!/bin/sh\nprintf called >> forge-calls\nprintf malformed\n",
    )
    .unwrap();
    let (status, op) = f
        .json(Method::POST, &path, OWNER, Some("uncertain"), json!({}))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{op}");
    let interrupted = f.finished(op["id"].as_str().unwrap());
    assert_eq!(
        interrupted.state,
        OperationState::Interrupted,
        "{interrupted:?}"
    );
    assert_eq!(interrupted.error.unwrap().code, "pr_effect_uncertain");
    let (status, retry) = f
        .json(Method::POST, &path, OWNER, Some("uncertain"), json!({}))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{retry}");
    assert_eq!(retry["id"], op["id"]);
    assert_eq!(retry["state"], "interrupted");
    assert_eq!(
        std::fs::read_to_string(case._repo.root.join("forge-calls")).unwrap(),
        "calledcalled"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_acl_rejects_foreign_reviewers_and_other_creators() {
    let case = Case::new("printf '{}'", true);
    let (wid, sid) = case.writer().await;
    let f = &case.fixture;
    let root = format!("/v1/workspaces/{wid}/sessions/{sid}");
    let (status, _) = f
        .json(
            Method::GET,
            &format!("{root}/runtime"),
            FOREIGN,
            None,
            Value::Null,
        )
        .await;
    assert!(matches!(
        status,
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
    ));
    let revision = f.detail(&wid).await["revision"].clone();
    let (status, grant) = f
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/members/editor"),
            OWNER,
            Some("grant-editor"),
            json!({"role":"editor","revision":revision}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{grant}");
    for endpoint in ["pr", "terminals"] {
        let (status, error) = f
            .json(
                Method::POST,
                &format!("{root}/{endpoint}"),
                EDITOR,
                Some(endpoint),
                json!({}),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
        assert_eq!(error["error"]["code"], "session_subject_required");
    }
    let candidate = f.app.repos["code"]
        .yard
        .branch(&sid)
        .unwrap()
        .info()
        .candidate
        .as_ref()
        .unwrap()
        .commit
        .clone();
    let (status,reviewer)=f.json(Method::POST,&format!("/v1/workspaces/{wid}/sessions"),OWNER,Some("reviewer"),json!({"name":"Reviewer","prompt":"Review only","harness":"codex-acp","access":"reviewer","candidate":candidate})).await;
    assert_eq!(status, StatusCode::OK, "{reviewer}");
    let done = f.finished(reviewer["id"].as_str().unwrap());
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let review = reviewer["session_id"].as_str().unwrap();
    for endpoint in ["pr", "terminals", "runtime/actions/destroy"] {
        let (status, error) = f
            .json(
                Method::POST,
                &format!("/v1/workspaces/{wid}/sessions/{review}/{endpoint}"),
                OWNER,
                Some(endpoint),
                json!({}),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
        assert_eq!(error["error"]["code"], "writer_required");
    }
    assert!(!case._repo.root.join("forge-calls").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_owner_terminal_and_archive_use_current_workspace_state() {
    let case = Case::new("printf '{}'", true);
    let (wid, sid) = case.writer().await;
    let f = &case.fixture;
    let root = format!("/v1/workspaces/{wid}/sessions/{sid}");
    let (status, state) = f
        .json(
            Method::GET,
            &format!("{root}/runtime"),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{state}");
    assert_eq!(state["state"], "local");
    let (status, unsupported) = f
        .json(
            Method::POST,
            &format!("{root}/runtime/actions/destroy"),
            OWNER,
            Some("destroy-local"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{unsupported}");
    assert_eq!(unsupported["error"]["code"], "unsupported");
    let (status, terminal) = f
        .json(
            Method::POST,
            &format!("{root}/terminals"),
            OWNER,
            Some("terminal"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{terminal}");
    let terminal_path = format!("{root}/terminals/{}", terminal["id"].as_str().unwrap());
    let revision = f.detail(&wid).await["revision"].clone();
    let (status, busy) = f
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/actions/archive"),
            OWNER,
            Some("archive-busy"),
            json!({"revision":revision}),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{busy}");
    assert_eq!(busy["error"]["code"], "workspace_busy");
    let (status, closed) = f
        .json(
            Method::POST,
            &format!("{terminal_path}/close"),
            OWNER,
            None,
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    let before = f
        .json(
            Method::GET,
            &format!("{root}/messages"),
            OWNER,
            None,
            Value::Null,
        )
        .await
        .1;
    for action in ["archive", "restore"] {
        let revision = f.detail(&wid).await["revision"].clone();
        let (status, operation) = f
            .json(
                Method::POST,
                &format!("/v1/workspaces/{wid}/actions/{action}"),
                OWNER,
                Some(action),
                json!({"revision":revision}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{operation}");
        let finished = f.finished(operation["id"].as_str().unwrap());
        assert_eq!(finished.state, OperationState::Succeeded, "{finished:?}");
    }
    let after = f
        .json(
            Method::GET,
            &format!("{root}/messages"),
            OWNER,
            None,
            Value::Null,
        )
        .await
        .1;
    assert_eq!(before["messages"], after["messages"]);
    let repo = f.app.repos.get("code").unwrap();
    let conn = rusqlite::Connection::open(repo.yard.root().join(".branchyard/state.db")).unwrap();
    conn.execute("UPDATE steps SET intent=json_set(intent,'$.host','foreign-worker-boot') WHERE branch=?1 AND step='start'",[&sid]).unwrap();
    let (status, state) = f
        .json(
            Method::GET,
            &format!("{root}/runtime"),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{state}");
    assert_eq!(state["state"], "unavailable");
    assert!(state["reason"].as_str().unwrap().contains("worker"));
    let (status, error) = f
        .json(
            Method::POST,
            &format!("{root}/terminals"),
            OWNER,
            Some("foreign-terminal"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{error}");
    assert_eq!(error["error"]["code"], "runtime_owner_unavailable");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admission_cas_rejects_revoked_grants_without_queuing_a_forge_effect() {
    let case = Case::new("printf '{}'", false);
    let (wid, sid) = case.writer().await;
    let f = &case.fixture;
    let principal = f
        .app
        .credentials
        .verify(Some(&format!("Bearer {OWNER}")))
        .unwrap()
        .clone();
    let caller = Caller::new(principal);
    let target =
        super::super::services::target(&f.app, &caller, &wid, &sid, "merge", true).unwrap();
    let member = case.row(&wid);
    let mut body = member.body.clone();
    body["role"] = json!("viewer");
    f.app
        .registry
        .console_mutate(&[super::super::update(member, body)])
        .unwrap();
    let command = super::super::ConsoleWork::PullRequest {
        workspace_id: wid.clone(),
        session_id: sid,
        command: PrCommand::Refresh,
        owner_host: f.app.registry.worker().host.clone(),
    };
    let error = super::super::services::queue(
        &f.app,
        &caller,
        target,
        command,
        json!({"action":"refresh"}),
        "revoked-cas",
    )
    .unwrap_err();
    assert_eq!(error.body.code, "revision_conflict");
    assert!(!case._repo.root.join("forge-calls").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_remote_runtime_actions_keep_native_state_and_durable_results() {
    use branchyard::{Provider, SandboxKeep, SandboxOptions};
    let case = Case::new("printf '{}'", false);
    let provider = Arc::new(branchyard_sandbox::fake::FakeProvider::live(
        Box::new(branchyard_runtime::LocalProvider::new()),
        case._repo.dir.join("fake-runtime"),
    ));
    case.fixture.app.repos["code"]
        .yard
        .use_sandbox_provider(provider.clone());
    let backend = Provider::microsandbox(SandboxOptions {
        image: "fixture:1".into(),
        keep: SandboxKeep::Pause,
        ..SandboxOptions::default()
    });
    let (wid, sid) = case.writer_on(Some(backend)).await;
    let f = &case.fixture;
    let base = format!("/v1/workspaces/{wid}/sessions/{sid}");
    for (action, state) in [
        ("resume", "running"),
        ("pause", "paused"),
        ("destroy", "absent"),
    ] {
        let (status, operation) = f
            .json(
                Method::POST,
                &format!("{base}/runtime/actions/{action}"),
                OWNER,
                Some(action),
                json!({}),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{operation}");
        let done = f.finished(operation["id"].as_str().unwrap());
        assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
        assert_eq!(
            done.result.as_ref().unwrap().runtime.as_ref().unwrap()["state"],
            state
        );
        let (status, replayed) = f
            .json(
                Method::POST,
                &format!("{base}/runtime/actions/{action}"),
                OWNER,
                Some(action),
                json!({}),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{replayed}");
        assert_eq!(replayed["id"], operation["id"]);
        let (status, stored) = f
            .json(
                Method::GET,
                &format!("/v1/operations/{}", operation["id"].as_str().unwrap()),
                OWNER,
                None,
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{stored}");
        assert_eq!(stored["result"]["runtime"]["state"], state);
    }
    let branch = f.app.repos["code"].yard.branch(&sid).unwrap();
    assert_eq!(
        branch.info().turns,
        1,
        "resource actions never replay a model turn"
    );
    assert!(branch.info().candidate.is_some());
    assert_eq!(
        provider
            .ops()
            .iter()
            .filter(|op| matches!(op, branchyard_sandbox::fake::FakeOp::Destroy { .. }))
            .count(),
        1
    );
    let (status,error)=f.json(Method::POST,&format!("/v1/workspaces/{wid}/sessions"),OWNER,Some("denied-provider"),json!({"name":"Denied","prompt":"say hello","harness":"codex-acp","access":"writer","provider":Provider::substrate(branchyard::SubstrateOptions::default())})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["error"]["code"], "provider_not_allowed");
    let (status,error)=f.json(Method::POST,&format!("/v1/workspaces/{wid}/sessions"),OWNER,Some("denied-ack"),json!({"name":"Unapproved","prompt":"say hello","harness":"antigravity","access":"writer","unapproved_tools":true})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["error"]["code"], "unapproved_tools_not_allowed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_revocation_withholds_forge_results_and_reconciliation_never_replays() {
    let case=Case::new("exec /usr/bin/python3 - <<'PY'\nimport os, signal\nsignal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGUSR1})\nwith open('forge-ready', 'w') as f:\n    f.write(str(os.getpid()))\nsignal.sigwait({signal.SIGUSR1})\nprint('{\"private_result\":\"never-disclose\"}')\nPY",false);
    let (wid, sid) = case.writer().await;
    let f = &case.fixture;
    let path = format!("/v1/workspaces/{wid}/sessions/{sid}/pr");
    let (status, operation) = f
        .json(Method::POST, &path, OWNER, Some("revoked-forge"), json!({}))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{operation}");
    let marker = case._repo.root.join("forge-ready");
    let mut pid = None;
    branchyard_testkit::wait::until("the owned forge subprocess to reach its barrier", || {
        match std::fs::read_to_string(&marker) {
            Ok(text) => {
                if let Ok(value) = text.parse::<u32>() {
                    pid = Some(value);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("cannot read the owned forge barrier: {error}"),
        }
        pid.is_some()
    });
    let grant = case.row(&wid);
    let mut body = grant.body.clone();
    body["role"] = json!("viewer");
    f.app
        .registry
        .console_mutate(&[super::super::update(grant, body)])
        .unwrap();
    assert!(std::process::Command::new("/bin/kill")
        .args(["-USR1", &pid.unwrap().to_string()])
        .status()
        .unwrap()
        .success());
    let completed = f.finished(operation["id"].as_str().unwrap());
    assert_eq!(
        completed.state,
        OperationState::Interrupted,
        "{completed:?}"
    );
    assert!(completed.result.is_none());
    assert_eq!(
        completed.error.as_ref().unwrap().code,
        "pr_effect_uncertain"
    );
    assert!(!serde_json::to_string(&completed)
        .unwrap()
        .contains("never-disclose"));
    let grant = case.row(&wid);
    let mut body = grant.body.clone();
    body["role"] = json!("owner");
    f.app
        .registry
        .console_mutate(&[super::super::update(grant, body)])
        .unwrap();
    let (status, replayed) = f
        .json(Method::POST, &path, OWNER, Some("revoked-forge"), json!({}))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{replayed}");
    assert_eq!(replayed["id"], operation["id"]);
    assert_eq!(replayed["state"], "interrupted");
    assert_eq!(
        std::fs::read_to_string(case._repo.root.join("forge-calls")).unwrap(),
        "called"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agy_acknowledgment_is_explicit_and_still_needs_operator_permission() {
    let case = Case::with_ack("printf '{}'", false, true);
    let (wid, _) = case.writer().await;
    let f = &case.fixture;
    let (status,operation)=f.json(Method::POST,&format!("/v1/workspaces/{wid}/sessions"),OWNER,Some("explicit-ack"),json!({"name":"AGY","prompt":"say hello","harness":"antigravity","access":"writer","unapproved_tools":true})).await;
    assert_eq!(status, StatusCode::OK, "{operation}");
    let done = f.finished(operation["id"].as_str().unwrap());
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let session = f
        .app
        .registry
        .console_get(
            Collection::Sessions,
            operation["session_id"].as_str().unwrap(),
        )
        .unwrap()
        .unwrap();
    let branch = f.app.repos["code"]
        .yard
        .branch(session.body["branch"].as_str().unwrap())
        .unwrap();
    assert_eq!(branch.info().harness, "antigravity");
    assert_eq!(branch.info().turns, 1);
    let (status, sent) = f
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/sessions/{}/messages", session.id),
            OWNER,
            Some("continue-ack"),
            json!({"prompt":"Continue checking"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    let done = f.finished(sent["id"].as_str().unwrap());
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    assert_eq!(
        f.app.repos["code"]
            .yard
            .branch(session.body["branch"].as_str().unwrap())
            .unwrap()
            .info()
            .turns,
        2
    );
    let (status,refused)=f.json(Method::POST,&format!("/v1/workspaces/{wid}/sessions"),OWNER,Some("reviewer-cannot-bypass"),json!({"name":"Unenforced review","prompt":"Review only","harness":"antigravity","access":"reviewer","unapproved_tools":true})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    assert_eq!(refused["error"]["code"], "unsupported");
}
