//! Socket-free HTTP and durable worker regressions, using only the fake agent.

mod discovery;
mod services;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{Method, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt as _;

use crate::api::{App, Caller};
use crate::auth::Credentials;
use crate::config::{Config, Principal, Token};
use crate::console_store::{Collection, Mutation};
use crate::ops::{Options, Registry};
use crate::store::MemoryStore;

const OWNER: &str = "console-inprocess-owner-token";
const EDITOR: &str = "console-inprocess-editor-token";
const FOREIGN: &str = "console-inprocess-foreign-token";
const RESTRICTED: &str = "console-inprocess-restricted-token";
const TOKENONLY: &str = "console-inprocess-tokenonly-token";

/// Seed inert native reservations for fake-agent turns in a socket-denied test.
/// The fake agent never listens on these numbers. Each fixture owns its SQLite
/// database, so this allocates no socket and performs no free-port race.
struct ReservedFakeAgentPorts(Arc<App>);

impl ReservedFakeAgentPorts {
    fn reserve(&self, workspace_id: &str, session_id: &str) -> Result<(), crate::error::ApiError> {
        if self.0.config.database.is_some() {
            return Err(crate::error::ApiError::internal(
                "fake-agent port fixtures require an isolated SQLite native store",
            ));
        }
        let workspace = super::get(&self.0, Collection::Workspaces, workspace_id)?;
        let repo = super::runtime::resolve(&self.0, &workspace)?;
        let path = repo.yard.root().join(".branchyard/state.db");
        let mut conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )
        .map_err(|error| crate::error::ApiError::internal(error.to_string()))?;
        conn.busy_timeout(Duration::from_secs(30))
            .map_err(|error| crate::error::ApiError::internal(error.to_string()))?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| crate::error::ApiError::internal(error.to_string()))?;
        let port: i64 = tx
            .query_row("SELECT COALESCE(MAX(port),19999)+1 FROM ports", [], |row| {
                row.get(0)
            })
            .map_err(|error| crate::error::ApiError::internal(error.to_string()))?;
        if !(20000..=29999).contains(&port) {
            return Err(crate::error::ApiError::internal(
                "fake-agent port fixture exhausted its inert reservation range",
            ));
        }
        let now = branchyard::store_codec::to_db("reserved_ms", branchyard_support::time::now_ms())
            .map_err(|error| crate::error::ApiError::internal(error.to_string()))?;
        tx.execute(
            "INSERT OR IGNORE INTO ports(port,branch,reserved_ms) VALUES(?1,?2,?3)",
            rusqlite::params![port, session_id, now],
        )
        .map_err(|error| crate::error::ApiError::internal(error.to_string()))?;
        tx.commit()
            .map_err(|error| crate::error::ApiError::internal(error.to_string()))?;
        Ok(())
    }
}

impl crate::ops::Executor for ReservedFakeAgentPorts {
    fn execute(
        &self,
        operation: &crate::store::StoredOperation,
        work: &Value,
    ) -> crate::ops::Finished {
        if let Ok(crate::work::Work::Console {
            command:
                super::ConsoleWork::Start {
                    workspace_id,
                    session_id,
                    ..
                },
            ..
        }) = serde_json::from_value::<crate::work::Work>(work.clone())
        {
            if let Err(error) = self.reserve(&workspace_id, &session_id) {
                return crate::ops::Finished {
                    result: Err(*error.body),
                    end_cursor: None,
                };
            }
        }
        crate::ops::Executor::execute(&crate::work::AppExecutor(self.0.clone()), operation, work)
    }
}

struct Fixture {
    app: Arc<App>,
    _scratch: branchyard_testkit::Scratch,
    _shutdown_sender: tokio::sync::watch::Sender<bool>,
}

impl Fixture {
    fn new(start: bool) -> Self {
        let scratch = branchyard_testkit::Scratch::new("console-inprocess");
        let mut config = Config::new(scratch.path().join("data"));
        config.managed_knowledge.enabled = true;
        config.harness_commands.insert(
            "gemini-cli".into(),
            vec![branchyard_testkit::fake_agent_here().display().to_string()],
        );
        for (name, secret) in [
            ("owner", OWNER),
            ("editor", EDITOR),
            ("foreign", FOREIGN),
            ("restricted", RESTRICTED),
        ] {
            config.tokens.push(Token {
                name: name.into(),
                secret: secret.into(),
            });
            let mut principal = Principal::default_for(name);
            if name == "foreign" {
                principal.tenant = "foreign".into();
            }
            if name == "restricted" {
                principal.repos = Some(["app".into()].into_iter().collect());
            }
            config.principals.insert(name.into(), principal);
        }
        config.tokens.push(Token {
            name: "tokenonly".into(),
            secret: TOKENONLY.into(),
        });
        let mut options = Options::new(vec![super::MANAGED_REPO.into()]);
        options.poll = Duration::from_millis(10);
        let registry = Registry::open(Box::<MemoryStore>::default(), options).unwrap();
        let (shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
        let hub = crate::triggers::dispatch::Hub::new(
            Arc::new(crate::triggers::store::SqliteTriggers::memory()),
            config.triggers.clone(),
            "http://inprocess.invalid".into(),
        );
        let app = Arc::new(App {
            #[cfg(target_os = "linux")]
            preview: Arc::new(crate::preview::tests::fake_service(
                &scratch,
                "printf '%s\\n' '{\"status\":\"passed\",\"kind\":\"table\",\"format\":\"csv\",\"columns\":[\"value\"],\"rows\":[[\"=1+1\"]],\"offset\":0,\"next_offset\":null,\"warnings\":[]}'",
            )),
            #[cfg(not(target_os = "linux"))]
            preview: Arc::new(crate::preview::PreviewService::new(scratch.join("previews")).unwrap()),
            repos: BTreeMap::new(),
            registry,
            credentials: Credentials::new(config.all_credentials()),
            config,
            shutdown,
            storage_idem: Default::default(),
            triggers: Arc::new(hub),
            companion: None,
            sync: None,
            runtime: Default::default(),
            pull_requests: Default::default(),
        });
        let fixture = Self {
            app,
            _scratch: scratch,
            _shutdown_sender: shutdown_sender,
        };
        if start {
            fixture.start();
        }
        fixture
    }

    fn start(&self) {
        if self.app.runtime.get().is_none() {
            super::services::install(&self.app).unwrap();
        }
        self.app
            .registry
            .start(Arc::new(ReservedFakeAgentPorts(self.app.clone())))
            .unwrap();
    }

    async fn bytes(
        &self,
        method: Method,
        path: &str,
        token: &str,
        key: Option<&str>,
        headers: &[(&str, String)],
        body: Vec<u8>,
    ) -> (StatusCode, Vec<u8>) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {token}"));
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        let response = crate::api::router(self.app.clone())
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    async fn json(
        &self,
        method: Method,
        path: &str,
        token: &str,
        key: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let (status, data) = self
            .bytes(
                method,
                path,
                token,
                key,
                &[("content-type", "application/json".into())],
                serde_json::to_vec(&body).unwrap(),
            )
            .await;
        (status, serde_json::from_slice(&data).unwrap())
    }

    fn finished(&self, id: &str) -> branchyard_client::api::Operation {
        branchyard_testkit::wait::until("the in-process durable operation to finish", || {
            self.app
                .registry
                .get(id)
                .unwrap()
                .is_some_and(|operation| operation.state.is_terminal())
        });
        self.app.registry.get(id).unwrap().unwrap()
    }

    async fn workspace(&self) -> String {
        let (status,project)=self.json(Method::POST,"/v1/projects",OWNER,Some("project"),json!({"name":"Knowledge review","source":{"kind":"knowledge"},"result_mode":"artifacts"})).await;
        assert_eq!(status, StatusCode::OK, "{project}");
        let (status, operation) = self
            .json(
                Method::POST,
                "/v1/workspaces",
                OWNER,
                Some("workspace"),
                json!({"project_id":project["id"],"title":"Evidence synthesis"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{operation}");
        let (replayed_status, replayed) = self
            .json(
                Method::POST,
                "/v1/workspaces",
                OWNER,
                Some("workspace"),
                json!({"project_id":project["id"],"title":"Evidence synthesis"}),
            )
            .await;
        assert_eq!(replayed_status, StatusCode::OK, "{replayed}");
        assert_eq!(replayed["id"], operation["id"]);
        assert_eq!(
            self.app
                .registry
                .console_list(Collection::Workspaces, "default")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            self.finished(operation["id"].as_str().unwrap()).state,
            branchyard_client::api::OperationState::Succeeded
        );
        operation["workspace_id"].as_str().unwrap().into()
    }

    async fn detail(&self, wid: &str) -> Value {
        let (status, detail) = self
            .json(
                Method::GET,
                &format!("/v1/workspaces/{wid}"),
                OWNER,
                None,
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        detail
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(runtime) = self.app.runtime.get() {
            runtime.close_all();
        }
        self.app.registry.close();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn knowledge_sources_native_deliverable_acceptance_and_revocation_without_sockets() {
    let fixture = Fixture::new(true);
    let wid = fixture.workspace().await;
    let (restricted_status, restricted) = fixture
        .json(
            Method::POST,
            "/v1/projects",
            RESTRICTED,
            Some("restricted-project"),
            json!({"name":"Denied","source":{"kind":"knowledge"},"result_mode":"artifacts"}),
        )
        .await;
    assert_eq!(restricted_status, StatusCode::FORBIDDEN, "{restricted}");
    let initial = fixture.detail(&wid).await;
    let original = b"Immutable evidence provided before any native turn.";
    let source_path = format!("/v1/workspaces/{wid}/sources");
    let (status, data) = fixture
        .bytes(
            Method::POST,
            &source_path,
            OWNER,
            Some("upload"),
            &[
                ("content-type", "text/plain".into()),
                ("x-source-title", "Original evidence".into()),
                ("x-workspace-revision", initial["revision"].to_string()),
            ],
            original.to_vec(),
        )
        .await;
    let source: Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(status, StatusCode::OK, "{source}");
    let (status, downloaded) = fixture
        .bytes(
            Method::GET,
            &format!("{source_path}/{}/content", source["id"].as_str().unwrap()),
            OWNER,
            None,
            &[],
            Vec::new(),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(downloaded, original);
    let mut contract=branchyard::ResultContract::deliverable(json!({"type":"object","required":["answer"],"properties":{"answer":{"type":"string"}},"additionalProperties":false})).unwrap();
    contract.capture = Some(branchyard::ResultCapture::new(
        ".cache/outputs/artifacts.json",
    ));
    let prompt=concat!("SH mkdir -p .cache/outputs && printf %s 'checked evidence' > .cache/outputs/evidence.txt && ","printf %s '{\"schema_version\":1,\"files\":[{\"path\":\"evidence.txt\",\"media_type\":\"text/plain\",\"sha256\":\"a06580d6ceda176885b7d46ba52084b1743497efd00763c5491c4fbeea45efe3\"}]}' > .cache/outputs/artifacts.json && ","printf '\\n```json\\n{\"answer\":\"checked\"}\\n```\\n'");
    let (status,started)=fixture.json(Method::POST,&format!("/v1/workspaces/{wid}/sessions"),OWNER,Some("writer"),json!({"name":"Evidence writer","prompt":prompt,"harness":"gemini-cli","access":"writer","sources":[source["id"]],"result_contract":contract})).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let id = started["id"].as_str().unwrap();
    let finished = fixture.finished(id);
    assert_eq!(
        finished.state,
        branchyard_client::api::OperationState::Succeeded,
        "{finished:?}"
    );
    let workspace_record = fixture
        .app
        .registry
        .console_get(Collection::Workspaces, &wid)
        .unwrap()
        .unwrap();
    let native = super::runtime::resolve(&fixture.app, &workspace_record).unwrap();
    let native_branch = native
        .yard
        .branch(started["session_id"].as_str().unwrap())
        .unwrap();
    let copied = native_branch.info().worktree.join(format!(
        ".console-inputs/{}/{}/{}",
        wid,
        started["session_id"].as_str().unwrap(),
        source["id"].as_str().unwrap()
    ));
    assert_eq!(std::fs::read(copied).unwrap(), original);
    let (status, operation) = fixture
        .json(
            Method::GET,
            &format!("/v1/operations/{id}"),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(operation["workspace_id"], wid);
    assert_eq!(operation["session_id"], started["session_id"]);
    let (status, recovered) = fixture
        .json(
            Method::GET,
            "/v1/operations?idempotency_key=writer",
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{recovered}");
    assert_eq!(recovered["id"], started["id"]);
    assert_eq!(recovered["workspace_id"], wid);
    assert_eq!(recovered["session_id"], started["session_id"]);
    let (status, messages) = fixture
        .json(
            Method::GET,
            &format!(
                "/v1/workspaces/{wid}/sessions/{}/messages",
                started["session_id"].as_str().unwrap()
            ),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(messages["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["role"] == "user"));
    let (status, artifacts) = fixture
        .json(
            Method::GET,
            &format!("/v1/workspaces/{wid}/artifacts"),
            OWNER,
            None,
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{artifacts}");
    assert_eq!(artifacts["artifacts"].as_array().unwrap().len(), 1);
    let aid = artifacts["artifacts"][0]["id"].as_str().unwrap();
    #[cfg(target_os = "linux")]
    {
        let (status, preview) = fixture
            .json(
                Method::GET,
                &format!("/v1/workspaces/{wid}/artifacts/{aid}/preview"),
                OWNER,
                None,
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{preview}");
        assert_eq!(preview["kind"], "table");
        let (status, _) = fixture
            .bytes(
                Method::GET,
                &format!("/v1/workspaces/{wid}/artifacts/{aid}/preview/assets/page-1.png"),
                OWNER,
                None,
                &[],
                Vec::new(),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    let current = fixture.detail(&wid).await;
    assert_eq!(current["sessions"][0]["status"], "no_changes");
    assert_eq!(
        current["sessions"][0]["result"],
        json!({"answer":"checked"})
    );
    assert!(current["sessions"][0]["result_event"].is_u64());
    assert!(current["sessions"][0]["native_session"].is_string());
    let (status, accepted) = fixture
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/finalize"),
            OWNER,
            Some("accept"),
            json!({"revision":current["revision"],"artifacts":[aid]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["artifacts"], json!([aid]));
    let detail = fixture.detail(&wid).await;
    let (status, member) = fixture
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/members/tokenonly"),
            OWNER,
            Some("tokenonly-member"),
            json!({"revision":detail["revision"],"role":"viewer"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{member}");
    let (status, _) = fixture
        .bytes(Method::GET, &source_path, TOKENONLY, None, &[], Vec::new())
        .await;
    assert_eq!(status, StatusCode::OK);
    let member_path = format!("/v1/workspaces/{wid}/members/editor");
    let detail = fixture.detail(&wid).await;
    let (status, member) = fixture
        .json(
            Method::POST,
            &member_path,
            OWNER,
            Some("member"),
            json!({"revision":detail["revision"],"role":"editor"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{member}");
    let owner_session = started["session_id"].as_str().unwrap();
    let owner_messages = format!("/v1/workspaces/{wid}/sessions/{owner_session}/messages");
    let (status, refused) = fixture
        .json(
            Method::POST,
            &owner_messages,
            EDITOR,
            Some("editor-continuation"),
            json!({"prompt":"Continue with another subject's account"}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"]["code"], "session_subject_required");
    let (status, refused) = fixture
        .json(
            Method::POST,
            &owner_messages,
            EDITOR,
            Some("editor-steering"),
            json!({"prompt":"Steer another subject's account","action":"steer"}),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    assert_eq!(refused["error"]["code"], "session_subject_required");
    let detail = fixture.detail(&wid).await;
    let (status, comment) = fixture
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/comments"),
            EDITOR,
            Some("comment"),
            json!({"revision":detail["revision"],"text":"Checked original evidence."}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{comment}");
    assert_eq!(comment["subject"], "editor");
    assert_eq!(comment["credential"], "editor");
    let detail = fixture.detail(&wid).await;
    let (status, removed) = fixture
        .json(
            Method::DELETE,
            &member_path,
            OWNER,
            Some("remove"),
            json!({"revision":detail["revision"]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    for token in [EDITOR, FOREIGN, RESTRICTED] {
        for path in [
            format!("/v1/workspaces/{wid}"),
            format!("/v1/operations/{id}"),
            format!("/v1/workspaces/{wid}/artifacts/{aid}/content"),
            format!(
                "/v1/repos/{}/branches/{}/artifacts/{aid}/content",
                super::MANAGED_REPO,
                started["session_id"].as_str().unwrap()
            ),
            format!("/v1/repos/{}/task-records", super::MANAGED_REPO),
        ] {
            let (status, _) = fixture
                .bytes(Method::GET, &path, token, None, &[], Vec::new())
                .await;
            assert!(
                matches!(status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND),
                "{path}: {status}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_initialization_rechecks_revoked_owner_before_filesystem_effects() {
    let fixture = Fixture::new(false);
    let (status,project)=fixture.json(Method::POST,"/v1/projects",OWNER,Some("queued-project"),json!({"name":"Queued knowledge","source":{"kind":"knowledge"},"result_mode":"artifacts"})).await;
    assert_eq!(status, StatusCode::OK);
    let (status, operation) = fixture
        .json(
            Method::POST,
            "/v1/workspaces",
            OWNER,
            Some("queued-workspace"),
            json!({"project_id":project["id"],"title":"Reserved only"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{operation}");
    let wid = operation["workspace_id"].as_str().unwrap();
    let id = operation["id"].as_str().unwrap();
    let workspace = fixture
        .app
        .registry
        .console_get(Collection::Workspaces, wid)
        .unwrap()
        .unwrap();
    let reservation: branchyard::tasks::TaskReservation =
        serde_json::from_value(workspace.body["_allocation"]["reservation"].clone()).unwrap();
    assert!(!reservation.work_root().exists());
    let member = fixture
        .app
        .registry
        .console_get(Collection::Members, &format!("{wid}:owner"))
        .unwrap()
        .unwrap();
    fixture
        .app
        .registry
        .console_mutate(&[Mutation::Delete {
            collection: Collection::Members,
            id: member.id,
            tenant: member.tenant,
            expected: member.revision,
        }])
        .unwrap();
    fixture.start();
    let finished = fixture.finished(id);
    assert_eq!(
        finished.state,
        branchyard_client::api::OperationState::Failed
    );
    assert_eq!(finished.error.unwrap().code, "workspace_access_denied");
    assert!(!reservation.work_root().exists());
    let (status, _) = fixture
        .bytes(
            Method::GET,
            &format!("/v1/operations/{id}"),
            OWNER,
            None,
            &[],
            Vec::new(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(super::auth::authorize_workspace(
        &fixture.app,
        &Caller::new(Principal::default_for("owner")),
        wid,
        "read"
    )
    .is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_artifacts_check_publisher_and_streams_close_after_revocation() {
    let repository = branchyard_testkit::Repo::init("/bin/true");
    let mut fixture = Fixture::new(false);
    let yard = branchyard::Yard::open(&repository.root).unwrap();
    {
        let app = Arc::get_mut(&mut fixture.app).unwrap();
        app.config
            .repos
            .push(("app".into(), repository.root.clone()));
        let feed = crate::feed::Feed::open(yard.clone()).unwrap();
        app.repos.insert(
            "app".into(),
            crate::api::RepoState {
                name: "app".into(),
                yard: yard.clone(),
                feed: Arc::new(feed),
                wake: Arc::new(tokio::sync::Notify::new()),
            },
        );
    }
    for name in ["own", "foreign"] {
        let request = branchyard_client::api::TaskRequest {
            prompt: "Hello".into(),
            harness: Some("gemini-cli".into()),
            name: Some(name.into()),
            isolated: true,
            ..Default::default()
        };
        let options =
            crate::work::task_options(&fixture.app, &fixture.app.repos["app"], &request).unwrap();
        yard.task("Hello").options(options).run().unwrap();
    }
    let original = repository.dir.join("foreign-evidence.txt");
    std::fs::write(&original, b"foreign evidence").unwrap();
    let artifact = yard
        .publish_artifact(
            "foreign",
            &original,
            None,
            Some("text/plain".into()),
            BTreeMap::new(),
        )
        .unwrap();
    yard.share_artifact("foreign", &artifact.id, "own").unwrap();
    let owner = Caller::new(Principal::default_for("owner"));
    let make = |collection, id, body| super::insert(super::new(collection, id, &owner, body));
    fixture
        .app
        .registry
        .console_mutate(&[
            make(
                Collection::Workspaces,
                "workspace",
                json!({"repo":"app","task_id":"task"}),
            ),
            make(
                Collection::Workspaces,
                "foreign-workspace",
                json!({"repo":"app","task_id":"foreign-task"}),
            ),
            make(
                Collection::Members,
                "workspace:owner",
                json!({"workspace_id":"workspace","subject":"owner","role":"owner"}),
            ),
            make(
                Collection::Members,
                "workspace:editor",
                json!({"workspace_id":"workspace","subject":"editor","role":"editor"}),
            ),
            make(
                Collection::Sessions,
                "different-session-id",
                json!({"workspace_id":"workspace","branch":"own"}),
            ),
            make(
                Collection::Bindings,
                "app:own",
                json!({"workspace_id":"workspace","repo":"app","branch":"own"}),
            ),
            make(
                Collection::Bindings,
                "app:foreign",
                json!({"workspace_id":"foreign-workspace","repo":"app","branch":"foreign"}),
            ),
        ])
        .unwrap();
    let mut merger = Principal::default_for("owner");
    merger.scopes = ["merge".to_owned()].into_iter().collect();
    assert!(super::auth::authorize_legacy(
        &fixture.app,
        &Caller::new(merger),
        &Method::POST,
        "/v1/repos/app/branches/own/merge"
    )
    .is_ok());
    for path in [
        format!(
            "/v1/repos/app/branches/own/artifacts/{}/content",
            artifact.id
        ),
        format!("/v1/repos/app/branches/own/artifacts/{}", artifact.id),
        format!(
            "/v1/repos/app/branches/own/artifacts/{}/preview",
            artifact.id
        ),
        format!(
            "/v1/repos/app/branches/own/artifacts/{}/preview/assets/page-1.png",
            artifact.id
        ),
        "/v1/repos/app/branches/own/artifacts".into(),
    ] {
        let (status, _) = fixture
            .bytes(Method::GET, &path, OWNER, None, &[], Vec::new())
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }
    for suffix in ["preview", "preview/assets/page-1.png"] {
        let path = format!(
            "/v1/workspaces/workspace/artifacts/{}/{suffix}",
            artifact.id
        );
        let (status, _) = fixture
            .bytes(Method::GET, &path, OWNER, None, &[], Vec::new())
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
    // The legacy repository stream requires access to all bound workspaces.
    // Grant the second workspace so this stream can open before revocation.
    fixture
        .app
        .registry
        .console_mutate(&[make(
            Collection::Members,
            "foreign-workspace:editor",
            json!({"workspace_id":"foreign-workspace","subject":"editor","role":"viewer"}),
        )])
        .unwrap();
    let request = Request::builder()
        .uri("/v1/repos/app/events/stream")
        .header("authorization", format!("Bearer {EDITOR}"))
        .body(Body::empty())
        .unwrap();
    let response = crate::api::router(fixture.app.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    use axum::body::HttpBody as _;
    let mut stream_body = response.into_body();
    let first = tokio::time::timeout(
        Duration::from_secs(3),
        std::future::poll_fn(|cx| std::pin::Pin::new(&mut stream_body).poll_frame(cx)),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap()
    .into_data()
    .unwrap();
    assert!(String::from_utf8(first.to_vec())
        .unwrap()
        .contains("event: open"));

    let member = fixture
        .app
        .registry
        .console_get(Collection::Members, "workspace:editor")
        .unwrap()
        .unwrap();
    fixture
        .app
        .registry
        .console_mutate(&[Mutation::Delete {
            collection: Collection::Members,
            id: member.id,
            tenant: member.tenant,
            expected: member.revision,
        }])
        .unwrap();
    let ended =
        tokio::time::timeout(Duration::from_secs(3), to_bytes(stream_body, 1024 * 1024)).await;
    assert!(
        ended.is_ok(),
        "open legacy stream outlived revoked membership"
    );
    assert!(ended.unwrap().is_ok());
    let binding = fixture
        .app
        .registry
        .console_get(Collection::Bindings, "app:own")
        .unwrap()
        .unwrap();
    fixture
        .app
        .registry
        .console_mutate(&[Mutation::Delete {
            collection: Collection::Bindings,
            id: binding.id,
            tenant: binding.tenant,
            expected: binding.revision,
        }])
        .unwrap();
    let (status, _) = fixture
        .bytes(
            Method::GET,
            "/v1/repos/app/branches/own",
            OWNER,
            None,
            &[],
            Vec::new(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    fixture
        .app
        .registry
        .console_mutate(&[make(
            Collection::Bindings,
            "app:own",
            json!({"workspace_id":"workspace"}),
        )])
        .unwrap();
    let (status, _) = fixture
        .bytes(
            Method::GET,
            "/v1/repos/app/branches/own",
            OWNER,
            None,
            &[],
            Vec::new(),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "workspace_id alone is an incomplete resource binding"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_of_a_queued_session_is_durable_before_its_branch_exists() {
    struct GatedStart {
        native: ReservedFakeAgentPorts,
        started: std::sync::mpsc::SyncSender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl crate::ops::Executor for GatedStart {
        fn execute(
            &self,
            operation: &crate::store::StoredOperation,
            work: &Value,
        ) -> crate::ops::Finished {
            if matches!(
                serde_json::from_value::<crate::work::Work>(work.clone()),
                Ok(crate::work::Work::Console {
                    command: super::ConsoleWork::Start { .. },
                    ..
                })
            ) {
                self.started.send(()).unwrap();
                use branchyard_support::LockExt as _;
                self.release
                    .lock_recovering("the queued-session cancellation gate")
                    .recv_timeout(Duration::from_secs(30))
                    .unwrap();
            }
            crate::ops::Executor::execute(&self.native, operation, work)
        }
    }
    let fixture = Fixture::new(false);
    let (started, seen) = std::sync::mpsc::sync_channel(1);
    let (release, wait) = std::sync::mpsc::channel();
    fixture
        .app
        .registry
        .start(Arc::new(GatedStart {
            native: ReservedFakeAgentPorts(fixture.app.clone()),
            started,
            release: std::sync::Mutex::new(wait),
        }))
        .unwrap();
    let wid = fixture.workspace().await;
    let (status, start) = fixture.json(Method::POST, &format!("/v1/workspaces/{wid}/sessions"), OWNER, Some("queued-to-cancel"), json!({"name":"Cancelled writer","prompt":"WRITE forbidden.txt=must-not-run","harness":"gemini-cli","access":"writer"})).await;
    assert_eq!(status, StatusCode::OK, "{start}");
    branchyard_testkit::wait::until("the session to reach its pre-execution test gate", || {
        seen.try_recv().is_ok()
    });
    let sid = start["session_id"].as_str().unwrap();
    let (status, cancel) = fixture
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/sessions/{sid}/cancel"),
            OWNER,
            Some("cancel-before-start"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{cancel}");
    let cancelled = fixture.finished(cancel["id"].as_str().unwrap());
    assert_eq!(
        cancelled.state,
        branchyard_client::api::OperationState::Succeeded,
        "{cancelled:?}"
    );
    let row = fixture
        .app
        .registry
        .console_get(Collection::Sessions, sid)
        .unwrap()
        .unwrap();
    assert!(super::runtime::cancel_requested(&row));
    release.send(()).unwrap();
    let stopped = fixture.finished(start["id"].as_str().unwrap());
    assert_eq!(
        stopped.state,
        branchyard_client::api::OperationState::Failed,
        "{stopped:?}"
    );
    assert_eq!(stopped.error.unwrap().code, "session_cancelled");
    let workspace = fixture
        .app
        .registry
        .console_get(Collection::Workspaces, &wid)
        .unwrap()
        .unwrap();
    let repo = super::runtime::resolve(&fixture.app, &workspace).unwrap();
    assert!(matches!(
        repo.yard.branch(sid),
        Err(branchyard::Error::UnknownBranch(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_old_delayed_cancel_never_cancels_an_explicitly_resumed_turn() {
    struct DelayedCancel {
        native: ReservedFakeAgentPorts,
        gated: std::sync::atomic::AtomicBool,
        started: std::sync::mpsc::SyncSender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl crate::ops::Executor for DelayedCancel {
        fn execute(
            &self,
            operation: &crate::store::StoredOperation,
            work: &Value,
        ) -> crate::ops::Finished {
            let cancel = matches!(serde_json::from_value::<crate::work::Work>(work.clone()), Ok(crate::work::Work::Console {command:super::ConsoleWork::Control{control,..},..}) if control == "cancel");
            if cancel && !self.gated.swap(true, std::sync::atomic::Ordering::SeqCst) {
                self.started.send(()).unwrap();
                use branchyard_support::LockExt as _;
                self.release
                    .lock_recovering("the delayed cancellation gate")
                    .recv_timeout(Duration::from_secs(30))
                    .unwrap();
            }
            crate::ops::Executor::execute(&self.native, operation, work)
        }
    }
    let fixture = Fixture::new(false);
    let (started, seen) = std::sync::mpsc::sync_channel(1);
    let (release, wait) = std::sync::mpsc::channel();
    fixture
        .app
        .registry
        .start(Arc::new(DelayedCancel {
            native: ReservedFakeAgentPorts(fixture.app.clone()),
            gated: std::sync::atomic::AtomicBool::new(false),
            started,
            release: std::sync::Mutex::new(wait),
        }))
        .unwrap();
    let wid = fixture.workspace().await;
    let (status, start) = fixture.json(Method::POST, &format!("/v1/workspaces/{wid}/sessions"), OWNER, Some("before-delayed-cancel"), json!({"name":"Resumable writer","prompt":"WHOAMI","harness":"gemini-cli","access":"writer"})).await;
    assert_eq!(status, StatusCode::OK, "{start}");
    assert_eq!(
        fixture.finished(start["id"].as_str().unwrap()).state,
        branchyard_client::api::OperationState::Succeeded
    );
    let sid = start["session_id"].as_str().unwrap();
    let path = format!("/v1/workspaces/{wid}/sessions/{sid}");
    let (status, old) = fixture
        .json(
            Method::POST,
            &format!("{path}/cancel"),
            OWNER,
            Some("old-cancel"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{old}");
    branchyard_testkit::wait::until("the older cancellation to reach its execution gate", || {
        seen.try_recv().is_ok()
    });
    let (status, resumed) = fixture
        .json(
            Method::POST,
            &format!("{path}/messages"),
            OWNER,
            Some("explicit-resume"),
            json!({"prompt":"HANG","action":"resume"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{resumed}");
    let workspace = fixture
        .app
        .registry
        .console_get(Collection::Workspaces, &wid)
        .unwrap()
        .unwrap();
    let repo = super::runtime::resolve(&fixture.app, &workspace).unwrap();
    branchyard_testkit::wait::until("the explicit resumed turn to receive its prompt", || {
        repo.yard.branch(sid).is_ok_and(|branch| {
            branch.events().is_ok_and(|events| {
                events
                    .iter()
                    .filter(|event| matches!(event.activity, branchyard::Activity::Prompt(_)))
                    .count()
                    == 2
            })
        })
    });
    release.send(()).unwrap();
    let superseded = fixture.finished(old["id"].as_str().unwrap());
    assert_eq!(
        superseded.state,
        branchyard_client::api::OperationState::Succeeded,
        "{superseded:?}"
    );
    assert_eq!(
        repo.yard.branch(sid).unwrap().info().status,
        branchyard::BranchStatus::Running
    );
    let (status, latest) = fixture
        .json(
            Method::POST,
            &format!("{path}/cancel"),
            OWNER,
            Some("cancel-current-turn"),
            json!({}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{latest}");
    assert_eq!(
        fixture.finished(latest["id"].as_str().unwrap()).state,
        branchyard_client::api::OperationState::Succeeded
    );
    fixture.finished(resumed["id"].as_str().unwrap());
    assert_eq!(
        repo.yard.branch(sid).unwrap().info().status,
        branchyard::BranchStatus::Interrupted
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_session_policy_and_provisioning_retain_operator_controls() {
    let fixture = Fixture::new(true);
    let wid = fixture.workspace().await;
    let route = format!("/v1/workspaces/{wid}/sessions");
    let project_id = fixture
        .app
        .registry
        .console_get(Collection::Workspaces, &wid)
        .unwrap()
        .unwrap()
        .body["project_id"]
        .clone();
    for (key, path, request) in [
        (
            "project-host-path",
            "/v1/projects".to_owned(),
            json!({"name":"No paths","source":{"kind":"knowledge","path":"/ungranted/source"},"result_mode":"artifacts"}),
        ),
        (
            "workspace-host-path",
            "/v1/workspaces".to_owned(),
            json!({"project_id":project_id,"title":"No paths","path":"/ungranted/source"}),
        ),
        (
            "unsupported-session-provider",
            route.clone(),
            json!({"name":"No fallback","prompt":"Hello","harness":"gemini-cli","access":"writer","provider":"unimplemented-remote"}),
        ),
    ] {
        let (status, error) = fixture
            .json(Method::POST, &path, OWNER, Some(key), request)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
        assert_eq!(error["error"]["code"], "invalid_request");
    }

    for (key, provision, code) in [
        (
            "unknown-secret",
            json!({"secrets":[{"name":"UNCONFIGURED_TEST_SECRET"}]}),
            "secret_not_allowed",
        ),
        (
            "unapproved-mcp",
            json!({"mcp_servers":[{"name":"unapproved","command":"/bin/true"}]}),
            "command_not_allowed",
        ),
    ] {
        let (status,error)=fixture.json(Method::POST,&route,OWNER,Some(key),json!({"name":"Rejected session","prompt":"Hello","harness":"gemini-cli","access":"writer","provision":provision})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
        assert_eq!(error["error"]["code"], code);
    }
    assert!(fixture
        .app
        .registry
        .console_list(Collection::Sessions, "default")
        .unwrap()
        .is_empty());
    for (key, policy, expected_write) in [
        ("default-deny", Value::Null, false),
        ("explicit-allow", json!({"mode":"allow"}), true),
    ] {
        let mut request = json!({"name":key,"prompt":"PERMISSION WRITE marker.txt=permitted","harness":"gemini-cli","access":"writer","model":null,"effort":null,"policy":null,"provision":null,"sources":null,"artifacts":null,"result_contract":null});
        if !policy.is_null() {
            request["policy"] = policy;
        }
        let (status, started) = fixture
            .json(Method::POST, &route, OWNER, Some(key), request)
            .await;
        assert_eq!(status, StatusCode::OK, "{started}");
        let finished = fixture.finished(started["id"].as_str().unwrap());
        assert_eq!(
            finished.state,
            branchyard_client::api::OperationState::Succeeded,
            "{finished:?}"
        );
        let workspace = fixture
            .app
            .registry
            .console_get(Collection::Workspaces, &wid)
            .unwrap()
            .unwrap();
        let native = super::runtime::resolve(&fixture.app, &workspace).unwrap();
        let branch = native
            .yard
            .branch(started["session_id"].as_str().unwrap())
            .unwrap();
        let marker = branch.info().worktree.join("marker.txt");
        assert_eq!(
            marker.exists(),
            expected_write,
            "explicit policy must reach the native permission decision"
        );
        if expected_write {
            assert_eq!(std::fs::read_to_string(marker).unwrap(), "permitted\n");
        }
    }
}

#[tokio::test]
async fn a_managed_operation_without_its_atomic_binding_fails_closed() {
    let fixture = Fixture::new(true);
    let workspace_id = fixture.workspace().await;
    let workspace = fixture
        .app
        .registry
        .console_get(Collection::Workspaces, &workspace_id)
        .unwrap()
        .unwrap();
    let operation_id = workspace.body["_operation_id"].as_str().unwrap();
    let path = format!("/v1/operations/{operation_id}");
    let (status, _) = fixture
        .json(Method::GET, &path, OWNER, None, json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    let guard = fixture
        .app
        .registry
        .hold(
            super::MANAGED_REPO,
            &format!("workspace:{workspace_id}"),
            "the lifecycle lock regression",
        )
        .unwrap();
    let (locked, _) = fixture
        .json(
            Method::POST,
            &format!("/v1/workspaces/{workspace_id}/actions/restore"),
            OWNER,
            Some("locked-restore"),
            json!({"revision":workspace.revision}),
        )
        .await;
    assert_eq!(locked, StatusCode::CONFLICT);
    assert_eq!(
        fixture
            .app
            .registry
            .console_get(Collection::Workspaces, &workspace_id)
            .unwrap()
            .unwrap()
            .revision,
        workspace.revision
    );
    drop(guard);

    let binding = fixture
        .app
        .registry
        .console_get(Collection::Bindings, &format!("operation:{operation_id}"))
        .unwrap()
        .unwrap();
    fixture
        .app
        .registry
        .console_mutate(&[Mutation::Delete {
            collection: Collection::Bindings,
            id: binding.id,
            tenant: binding.tenant,
            expected: binding.revision,
        }])
        .unwrap();
    let (status, error) = fixture
        .json(Method::GET, &path, OWNER, None, json!({}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
    assert_eq!(error["error"]["code"], "workspace_access_denied");
}

#[tokio::test]
async fn initialization_completion_preserves_concurrent_sources_and_rejects_new_owners() {
    let fixture = Fixture::new(false);
    let original = super::new(
        Collection::Workspaces,
        "completion",
        &Caller::new(Principal::default_for("owner")),
        json!({"id":"completion","state":"pending","_operation_id":"initializing","_source_bytes":0}),
    );
    fixture
        .app
        .registry
        .console_mutate(&[super::insert(original.clone())])
        .unwrap();
    let mut uploaded = original.body.clone();
    uploaded["_source_bytes"] = json!(1234);
    fixture
        .app
        .registry
        .console_mutate(&[super::update(original.clone(), uploaded)])
        .unwrap();
    super::execution::complete_workspace_state(&fixture.app, &original, "initializing", "ready")
        .unwrap();
    let completed = fixture
        .app
        .registry
        .console_get(Collection::Workspaces, &original.id)
        .unwrap()
        .unwrap();
    assert_eq!(completed.body["_source_bytes"], 1234);
    assert_eq!(completed.body["state"], "ready");
    assert_eq!(completed.revision, 3);
    let mut restored = completed.body.clone();
    restored["_operation_id"] = json!("newer");
    restored["state"] = json!("restoring");
    fixture
        .app
        .registry
        .console_mutate(&[super::update(completed, restored)])
        .unwrap();
    assert_eq!(
        super::execution::complete_workspace_state(
            &fixture.app,
            &original,
            "initializing",
            "ready"
        )
        .unwrap_err()
        .body
        .code,
        "stale_workspace_operation"
    );
    assert_eq!(
        fixture
            .app
            .registry
            .console_get(Collection::Workspaces, &original.id)
            .unwrap()
            .unwrap()
            .body["state"],
        "restoring"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_candidate_review_is_read_only_and_frozen_when_the_writer_advances() {
    let fixture = Fixture::new(true);
    let (status, project) = fixture
        .json(
            Method::POST,
            "/v1/projects",
            OWNER,
            Some("code-review-project"),
            json!({"name":"Code review","source":{"kind":"knowledge"},"result_mode":"code"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{project}");
    let (status, allocation) = fixture
        .json(
            Method::POST,
            "/v1/workspaces",
            OWNER,
            Some("code-review-workspace"),
            json!({"project_id":project["id"],"title":"Frozen code review"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{allocation}");
    assert_eq!(
        fixture.finished(allocation["id"].as_str().unwrap()).state,
        branchyard_client::api::OperationState::Succeeded
    );
    let wid = allocation["workspace_id"].as_str().unwrap();
    let path = format!("/v1/workspaces/{wid}/sessions");
    let policy = branchyard_client::api::PolicySpec::allow_all();
    let (status, writer) = fixture.json(Method::POST, &path, OWNER, Some("candidate-writer"), json!({"name":"Code writer","prompt":"WRITE notes.txt=first","harness":"gemini-cli","access":"writer","policy":policy})).await;
    assert_eq!(status, StatusCode::OK, "{writer}");
    assert_eq!(
        fixture.finished(writer["id"].as_str().unwrap()).state,
        branchyard_client::api::OperationState::Succeeded
    );
    let writer_id = writer["session_id"].as_str().unwrap();
    let detail = fixture.detail(wid).await;
    let candidate = detail["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["id"] == writer_id)
        .unwrap()["candidate"]
        .as_str()
        .unwrap()
        .to_owned();
    for (key, arbitrary) in [
        ("review-head", "HEAD".to_owned()),
        ("review-rev-expression", format!("{candidate}~1")),
    ] {
        let (status, invalid) = fixture.json(Method::POST, &path, OWNER, Some(key), json!({"name":"Invalid review","prompt":"Review","harness":"gemini-cli","access":"reviewer","candidate":arbitrary})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid}");
    }
    let (status, review) = fixture.json(Method::POST, &path, OWNER, Some("frozen-code-review"), json!({"name":"Code reviewer","prompt":"PERMISSION WRITE forbidden.txt=must-not-write","harness":"gemini-cli","access":"reviewer","candidate":candidate,"policy":policy})).await;
    assert_eq!(status, StatusCode::OK, "{review}");
    assert_eq!(
        fixture.finished(review["id"].as_str().unwrap()).state,
        branchyard_client::api::OperationState::Succeeded
    );
    let review_id = review["session_id"].as_str().unwrap();
    let workspace = fixture
        .app
        .registry
        .console_get(Collection::Workspaces, wid)
        .unwrap()
        .unwrap();
    let repo = super::runtime::resolve(&fixture.app, &workspace).unwrap();
    let reviewer = repo.yard.branch(review_id).unwrap();
    assert_eq!(reviewer.info().base, candidate);
    assert_eq!(
        std::fs::read_to_string(reviewer.info().worktree.join("notes.txt")).unwrap(),
        "first\n"
    );
    assert!(!reviewer.info().worktree.join("forbidden.txt").exists());
    assert!(reviewer.events().unwrap().iter().any(|event|matches!(&event.activity,branchyard::Activity::Harness(branchyard::Event::MessageDelta{text,..}) if text.contains("denied"))));
    let (status, advanced) = fixture
        .json(
            Method::POST,
            &format!("{path}/{writer_id}/messages"),
            OWNER,
            Some("advance-writer-candidate"),
            json!({"prompt":"WRITE notes.txt=second"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{advanced}");
    assert_eq!(
        fixture.finished(advanced["id"].as_str().unwrap()).state,
        branchyard_client::api::OperationState::Succeeded
    );
    assert_eq!(
        std::fs::read_to_string(
            repo.yard
                .branch(writer_id)
                .unwrap()
                .info()
                .worktree
                .join("notes.txt")
        )
        .unwrap(),
        "second\n"
    );
    assert_eq!(
        std::fs::read_to_string(
            repo.yard
                .branch(review_id)
                .unwrap()
                .info()
                .worktree
                .join("notes.txt")
        )
        .unwrap(),
        "first\n"
    );
    assert_eq!(repo.yard.branch(review_id).unwrap().info().base, candidate);
    let detail = fixture.detail(wid).await;
    let sessions = detail["sessions"].as_array().unwrap();
    let current = sessions
        .iter()
        .find(|session| session["id"] == writer_id)
        .unwrap()["candidate"]
        .as_str()
        .unwrap();
    assert_ne!(current, candidate);
    let frozen = sessions
        .iter()
        .find(|session| session["id"] == review_id)
        .unwrap();
    assert_eq!(frozen["review_candidate"], candidate);
    assert_eq!(frozen["review_candidate_branch"], writer_id);
    let (status, stale) = fixture.json(Method::POST, &path, OWNER, Some("stale-candidate-review"), json!({"name":"Stale review","prompt":"Review","harness":"gemini-cli","access":"reviewer","candidate":candidate})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{stale}");
    let (status, other) = fixture
        .json(
            Method::POST,
            "/v1/workspaces",
            OWNER,
            Some("other-code-workspace"),
            json!({"project_id":project["id"],"title":"Another workspace"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{other}");
    assert_eq!(
        fixture.finished(other["id"].as_str().unwrap()).state,
        branchyard_client::api::OperationState::Succeeded
    );
    let (status, foreign) = fixture.json(Method::POST, &format!("/v1/workspaces/{}/sessions",other["workspace_id"].as_str().unwrap()), OWNER, Some("other-workspace-candidate"), json!({"name":"Foreign review","prompt":"Review","harness":"gemini-cli","access":"reviewer","candidate":current})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{foreign}");
}

#[tokio::test]
async fn presence_is_attributed_bounded_and_expiring_without_waiting() {
    let fixture = Fixture::new(true);
    let workspace = fixture.workspace().await;
    let path = format!("/v1/workspaces/{workspace}/presence");
    let (status, error) = fixture
        .json(
            Method::POST,
            &path,
            OWNER,
            Some("presence-invalid"),
            json!({"ttl_seconds":301}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
    let (status, active) = fixture
        .json(
            Method::POST,
            &path,
            OWNER,
            Some("presence-valid"),
            json!({"ttl_seconds":30,"subject":"forged"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{active}");
    assert_eq!(active["subject"], "owner");
    fixture
        .app
        .registry
        .console_mutate(&[super::insert(super::new(
            Collection::Presence,
            &format!("{workspace}:expired"),
            &Caller::new(Principal::default_for("owner")),
            json!({"workspace_id":workspace,"subject":"expired","expires_at_ms":0}),
        ))])
        .unwrap();
    let (status, listed) = fixture
        .json(Method::GET, &path, OWNER, None, json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["presence"].as_array().unwrap().len(), 1);
    assert_eq!(listed["presence"][0]["subject"], "owner");
    let (status, _) = fixture
        .json(Method::GET, &path, FOREIGN, None, json!({}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn console_admission_respects_a_negative_tenant_priority_ceiling() {
    let mut fixture = Fixture::new(false);
    Arc::get_mut(&mut fixture.app)
        .unwrap()
        .config
        .tenants
        .insert(
            "default".into(),
            crate::config::TenantPolicy {
                max_priority: Some(-5),
                ..Default::default()
            },
        );
    let (status,project)=fixture.json(Method::POST,"/v1/projects",OWNER,Some("priority-project"),json!({"name":"Prioritized research","source":{"kind":"knowledge"},"result_mode":"artifacts"})).await;
    assert_eq!(status, StatusCode::OK, "{project}");
    let (status, operation) = fixture
        .json(
            Method::POST,
            "/v1/workspaces",
            OWNER,
            Some("priority-workspace"),
            json!({"project_id":project["id"],"title":"Queued research"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{operation}");
    assert_eq!(operation["priority"], -5);
    assert_eq!(
        fixture
            .app
            .registry
            .get(operation["id"].as_str().unwrap())
            .unwrap()
            .unwrap()
            .priority,
        -5
    );
}

#[cfg(target_os = "linux")]
mod preview_http {
    use super::*;
    use branchyard_testkit::wait;

    #[tokio::test]
    async fn oversized_original_is_rejected_before_conversion() {
        let fixture = Fixture::new(true);
        let workspace = fixture.workspace().await;
        let source = upload(&fixture, &workspace, "oversized.csv", b"value\n1\n").await;
        let id = source["id"].as_str().unwrap();
        let metadata = fixture
            .app
            .registry
            .console_get(Collection::Sources, id)
            .unwrap()
            .unwrap();
        let mut body = metadata.body.clone();
        body["size_bytes"] = json!(crate::preview::MAX_INPUT + 1);
        fixture
            .app
            .registry
            .console_mutate(&[super::super::update(metadata, body)])
            .unwrap();
        let (status, error) = fixture
            .json(
                Method::GET,
                &format!("/v1/workspaces/{workspace}/sources/{id}/preview"),
                OWNER,
                None,
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error["error"]["code"], "preview_too_large");
        assert_eq!(
            std::fs::read_dir(fixture._scratch.join("cache"))
                .unwrap()
                .count(),
            0
        );
    }

    async fn upload(fixture: &Fixture, workspace: &str, title: &str, data: &[u8]) -> Value {
        let detail = fixture.detail(workspace).await;
        assert_eq!(detail["current_subject"], "owner");
        assert_eq!(detail["current_role"], "owner");
        assert!(detail["capabilities"]["max_upload_bytes"].as_u64().unwrap() > 0);
        assert_eq!(detail["capabilities"]["can_admin"], true);
        let (status, body) = fixture
            .bytes(
                Method::POST,
                &format!("/v1/workspaces/{workspace}/sources"),
                OWNER,
                Some(title),
                &[
                    ("content-type", "application/octet-stream".into()),
                    ("x-source-title", title.into()),
                    ("x-workspace-revision", detail["revision"].to_string()),
                ],
                data.to_vec(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn source_preview_preserves_original_and_requires_current_membership() {
        let fixture = Fixture::new(true);
        let workspace = fixture.workspace().await;
        let original = b"value\n=1+1\n";
        let source = upload(&fixture, &workspace, "inputs.csv", original).await;
        let base = format!(
            "/v1/workspaces/{workspace}/sources/{}",
            source["id"].as_str().unwrap()
        );
        let request = Request::builder()
            .uri(format!("{base}/preview?view=table&limit=2"))
            .header("authorization", format!("Bearer {OWNER}"))
            .body(Body::empty())
            .unwrap();
        let response = crate::api::router(fixture.app.clone())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        let body: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 2 * 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["kind"], "table");
        assert_eq!(body["rows"][0][0], "=1+1");
        let (status, bytes) = fixture
            .bytes(
                Method::GET,
                &format!("{base}/content"),
                OWNER,
                None,
                &[],
                Vec::new(),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, original);
        for suffix in ["/preview", "/preview/assets/page-1.png"] {
            let (status, _) = fixture
                .bytes(
                    Method::GET,
                    &format!("{base}{suffix}"),
                    EDITOR,
                    None,
                    &[],
                    Vec::new(),
                )
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
        }
        let member = fixture
            .app
            .registry
            .console_get(Collection::Members, &format!("{workspace}:owner"))
            .unwrap()
            .unwrap();
        fixture
            .app
            .registry
            .console_mutate(&[Mutation::Delete {
                collection: Collection::Members,
                id: member.id,
                tenant: member.tenant,
                expected: member.revision,
            }])
            .unwrap();
        for suffix in ["/preview", "/preview/assets/page-1.png"] {
            let (status, _) = fixture
                .bytes(
                    Method::GET,
                    &format!("{base}{suffix}"),
                    OWNER,
                    None,
                    &[],
                    Vec::new(),
                )
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
        }
    }

    #[tokio::test]
    async fn source_page_assets_are_authenticated_and_cannot_select_another_image() {
        let mut fixture = Fixture::new(false);
        let scratch = branchyard_testkit::Scratch::new("console-page-preview");
        let body = "mkdir \"$output/derived\"\nprintf '\\211PNG\\r\\n\\032\\n' > \"$output/derived/page-1.png\"\nprintf '%s\\n' '{\"status\":\"passed\",\"kind\":\"pages\",\"format\":\"pdf\",\"page\":1,\"pages\":1,\"image\":\"page-1.png\",\"width\":1,\"height\":1,\"warnings\":[]}'";
        Arc::get_mut(&mut fixture.app).unwrap().preview =
            Arc::new(crate::preview::tests::fake_service(&scratch, body));
        fixture.start();
        let workspace = fixture.workspace().await;
        let source = upload(
            &fixture,
            &workspace,
            "report.pdf",
            b"inert converter fixture",
        )
        .await;
        let base = format!(
            "/v1/workspaces/{workspace}/sources/{}/preview",
            source["id"].as_str().unwrap()
        );
        let (status, result) = fixture
            .json(Method::GET, &base, OWNER, None, Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["image"], "page-1.png");
        let (status, png) = fixture
            .bytes(
                Method::GET,
                &format!("{base}/assets/page-1.png"),
                OWNER,
                None,
                &[],
                Vec::new(),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(png, b"\x89PNG\r\n\x1a\n");
        let (status, _) = fixture
            .bytes(
                Method::GET,
                &format!("{base}/assets/page-2.png"),
                OWNER,
                None,
                &[],
                Vec::new(),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = fixture
            .bytes(
                Method::GET,
                &format!("{base}/assets/page-1.png"),
                FOREIGN,
                None,
                &[],
                Vec::new(),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn membership_revoked_during_conversion_blocks_the_result() {
        let mut fixture = Fixture::new(false);
        let scratch = branchyard_testkit::Scratch::new("console-preview-revocation");
        let entered = scratch.join("entered");
        let gate = scratch.join("release");
        assert!(std::process::Command::new("mkfifo")
            .arg(&gate)
            .status()
            .unwrap()
            .success());
        let body = format!("printf ready > '{}'\nread release < '{}'\nprintf '%s\\n' '{{\"status\":\"passed\",\"kind\":\"table\",\"format\":\"csv\",\"columns\":[\"value\"],\"rows\":[[\"1\"]],\"offset\":0,\"next_offset\":null,\"warnings\":[]}}'", entered.display(), gate.display());
        Arc::get_mut(&mut fixture.app).unwrap().preview =
            Arc::new(crate::preview::tests::fake_service(&scratch, &body));
        fixture.start();
        let workspace = fixture.workspace().await;
        let source = upload(&fixture, &workspace, "revocation.csv", b"value\n1\n").await;
        let request = Request::builder()
            .uri(format!(
                "/v1/workspaces/{workspace}/sources/{}/preview",
                source["id"].as_str().unwrap()
            ))
            .header("authorization", format!("Bearer {OWNER}"))
            .body(Body::empty())
            .unwrap();
        let router = crate::api::router(fixture.app.clone());
        let pending = tokio::spawn(async move { router.oneshot(request).await.unwrap() });
        wait::until("the fixture converter to reach the revocation gate", || {
            entered.exists()
        });
        let member = fixture
            .app
            .registry
            .console_get(Collection::Members, &format!("{workspace}:owner"))
            .unwrap()
            .unwrap();
        fixture
            .app
            .registry
            .console_mutate(&[Mutation::Delete {
                collection: Collection::Members,
                id: member.id,
                tenant: member.tenant,
                expected: member.revision,
            }])
            .unwrap();
        std::fs::write(gate, b"continue\n").unwrap();
        let response = pending.await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_named_sessions_admit_once_without_exposing_internal_revision_conflicts() {
    let fixture = Fixture::new(true);
    let wid = fixture.workspace().await;
    let barrier = std::sync::Barrier::new(4);
    let attempts = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|index| {
                let fixture = &fixture;
                let wid = &wid;
                let barrier = &barrier;
                scope.spawn(move || {
                    let mut headers = axum::http::HeaderMap::new();
                    headers.insert("idempotency-key", format!("parallel-{index}").parse().unwrap());
                    let body = json!({"name":format!("Writer {index}"),"prompt":"Say hello","harness":"gemini-cli","access":"writer"});
                    barrier.wait();
                    let result = super::runtime::start(&fixture.app, &Caller::new(Principal::default_for("owner")), wid, &headers, body.clone());
                    (headers, body, result)
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    let mut operation_ids = std::collections::BTreeSet::new();
    let mut session_ids = std::collections::BTreeSet::new();
    for (headers, body, result) in attempts {
        let admitted = result.unwrap();
        let replay = super::runtime::start(
            &fixture.app,
            &Caller::new(Principal::default_for("owner")),
            &wid,
            &headers,
            body,
        )
        .unwrap();
        assert_eq!(admitted["id"], replay["id"]);
        assert_eq!(admitted["session_id"], replay["session_id"]);
        operation_ids.insert(admitted["id"].as_str().unwrap().to_owned());
        session_ids.insert(admitted["session_id"].as_str().unwrap().to_owned());
    }
    assert_eq!(operation_ids.len(), 4);
    assert_eq!(session_ids.len(), 4);
    assert_eq!(
        fixture
            .app
            .registry
            .console_list(Collection::Sessions, "default")
            .unwrap()
            .len(),
        4
    );
}
