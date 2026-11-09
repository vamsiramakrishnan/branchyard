//! Execution presets through the real routes, queue and native engine, with
//! the fake agent and Substrate environments that are never reached.

use super::*;
use crate::config::ExecutionPreset;
use crate::console::Projection;
use axum::response::IntoResponse as _;
use branchyard_client::api::{BranchEvents, Operation, OperationList, OperationState};
use branchyard_client::workspace_api::ExecutionPresetList;

/// A server whose configuration and worker labels the test chose before it
/// started, with the registered repository `code` when asked.
struct Case {
    fixture: Fixture,
    _repo: Option<branchyard_testkit::Repo>,
}

impl Case {
    fn new(configure: impl FnOnce(&mut Config)) -> Case {
        Case::build(configure, false, &[])
    }

    fn build(configure: impl FnOnce(&mut Config), repository: bool, labels: &[&str]) -> Case {
        let mut fixture = Fixture::new(false);
        let app = Arc::get_mut(&mut fixture.app).unwrap();
        configure(&mut app.config);
        let mut repos = vec![super::super::MANAGED_REPO.to_owned()];
        let repo = repository.then(|| {
            let repo = branchyard_testkit::Repo::init("unused-by");
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
            repos.push("code".into());
            repo
        });
        let mut options = Options::new(repos);
        options.poll = Duration::from_millis(10);
        options.labels = labels.iter().map(|label| (*label).to_owned()).collect();
        app.registry = Registry::open(Box::<MemoryStore>::default(), options).unwrap();
        fixture.start();
        Case {
            fixture,
            _repo: repo,
        }
    }

    async fn presets(&self, token: &str, workspace: Option<&str>) -> (StatusCode, Value) {
        let path = match workspace {
            Some(id) => format!("/v1/execution-presets?workspace={id}"),
            None => "/v1/execution-presets".into(),
        };
        self.fixture
            .json(Method::GET, &path, token, None, Value::Null)
            .await
    }

    async fn start(&self, wid: &str, key: &str, fields: Value) -> (StatusCode, Value) {
        let mut body = json!({"name":"Analyst","prompt":"Summarise the backlog","harness":"gemini-cli","access":"writer"});
        body.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        self.fixture
            .json(
                Method::POST,
                &format!("/v1/workspaces/{wid}/sessions"),
                OWNER,
                Some(key),
                body,
            )
            .await
    }

    async fn repository_workspace(&self) -> String {
        let f = &self.fixture;
        let (status, project) = f
            .json(
                Method::POST,
                "/v1/projects",
                OWNER,
                Some("code-project"),
                json!({"name":"Code","source":{"kind":"repository","repo":"code"},"result_mode":"mixed"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{project}");
        let (status, operation) = f
            .json(
                Method::POST,
                "/v1/workspaces",
                OWNER,
                Some("code-workspace"),
                json!({"project_id":project["id"],"title":"Repository work"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{operation}");
        assert_eq!(
            f.finished(operation["id"].as_str().unwrap()).state,
            OperationState::Succeeded
        );
        operation["workspace_id"].as_str().unwrap().into()
    }

    async fn sessions(&self, wid: &str) -> Vec<Value> {
        let (status, sessions) = self
            .fixture
            .json(
                Method::GET,
                &format!("/v1/workspaces/{wid}/sessions"),
                OWNER,
                None,
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{sessions}");
        sessions["sessions"].as_array().unwrap().clone()
    }

    fn branch(&self, wid: &str, session: &str) -> Option<branchyard::Branch> {
        let workspace = super::super::get(&self.fixture.app, Collection::Workspaces, wid).unwrap();
        let repo = super::super::runtime::resolve(&self.fixture.app, &workspace).unwrap();
        repo.yard.branch(session).ok()
    }

    async fn read(&self, token: &str, path: &str) -> (StatusCode, String) {
        let (status, body) = self
            .fixture
            .bytes(Method::GET, path, token, None, &[], Vec::new())
            .await;
        (status, String::from_utf8(body).unwrap())
    }

    /// Repository `code`'s feed from its start to the head it had when the
    /// stream opened, as `token` reads it.
    async fn feed(&self, token: &str) -> String {
        use axum::body::HttpBody as _;
        let request = Request::builder()
            .uri("/v1/repos/code/events/stream?cursor=0")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = crate::api::router(self.fixture.app.clone())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body();
        let mut text = String::new();
        loop {
            let head = text
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .find_map(|data| serde_json::from_str::<Value>(data.trim()).ok()?["head"].as_u64());
            let last = text
                .lines()
                .filter_map(|line| line.strip_prefix("id:"))
                .filter_map(|id| id.trim().parse::<u64>().ok())
                .max();
            if head.is_some() && last >= head {
                return text;
            }
            let frame = tokio::time::timeout(
                Duration::from_secs(30),
                std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)),
            )
            .await
            .expect("the feed stalled before its head")
            .expect("the feed ended before its head")
            .unwrap();
            if let Ok(data) = frame.into_data() {
                text.push_str(std::str::from_utf8(&data).unwrap());
            }
        }
    }

    async fn grant(&self, wid: &str, subject: &str, role: &str) {
        let revision = self.fixture.detail(wid).await["revision"].clone();
        let (status, member) = self
            .fixture
            .json(
                Method::POST,
                &format!("/v1/workspaces/{wid}/members/{subject}"),
                OWNER,
                Some(&format!("grant-{subject}")),
                json!({"revision":revision,"role":role}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{member}");
    }
}

fn put(app: &App, collection: Collection, id: &str, body: Value) {
    let owner = Caller::new(Principal::default_for("owner"));
    app.registry
        .console_mutate(&[super::super::insert(super::super::new(
            collection, id, &owner, body,
        ))])
        .unwrap();
}

/// A session row whose own branch is `id`, with `preset` stored as a
/// session start stores the preset it resolved.
fn session_row(app: &App, id: &str, workspace: &str, preset: Value) {
    put(
        app,
        Collection::Sessions,
        id,
        json!({"id":id,"workspace_id":workspace,"branch":id,"_preset":preset}),
    );
}

fn operation(repo: &str, branches: &[&str], message: &str) -> Operation {
    serde_json::from_value(
        json!({"id":format!("op-{}", branches.join("-")),"repo":repo,
        "kind":"send","state":"running","branches":branches,"cursor":0,"created_at_ms":0,
        "error":{"code":"unavailable","message":message}}),
    )
    .unwrap()
}

async fn body_of(response: axum::response::Response) -> Vec<u8> {
    to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec()
}

fn presets(value: Value) -> Vec<ExecutionPreset> {
    serde_json::from_value(value).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_targets_require_the_server_resolved_preset() {
    let image = format!("private-registry/worker@sha256:{}", "a".repeat(64));
    let targets = [
        json!({"kind":"container","image":image}),
        json!({"kind":"agent_sandbox","context":"private-context","namespace":"branchyard-gvisor","image":image}),
        json!({"kind":"agent_sandbox","context":"private-context","namespace":"branchyard-microvm","runtime":"microvm","image":image}),
    ];
    let case = Case::new(|config| {
        config
            .allow_providers
            .extend(["container".into(), "agent_sandbox".into()]);
        config.execution_presets = targets
            .iter()
            .enumerate()
            .map(|(index, provider)| {
                serde_json::from_value(json!({
                    "id":format!("native-{index}"),"label":format!("Native {index}"),
                    "provider":provider,"harnesses":[{"harness":"gemini-cli"}]
                }))
                .unwrap()
            })
            .collect();
    });
    let wid = case.fixture.workspace().await;
    let (status, offered) = case.presets(OWNER, Some(&wid)).await;
    assert_eq!(status, StatusCode::OK, "{offered}");
    let caller = Caller::new(Principal::default_for("owner"));
    let workspace = super::super::get(&case.fixture.app, Collection::Workspaces, &wid).unwrap();
    let repo = super::super::runtime::resolve(&case.fixture.app, &workspace).unwrap();
    for (index, provider) in targets.iter().enumerate() {
        let id = format!("native-{index}");
        let shown = preset(&offered, &id);
        assert_eq!(shown["environment"]["state"], "unknown", "{shown}");
        let (status, refused) = case
            .start(&wid, &format!("raw-{index}"), json!({"provider":provider}))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
        assert_eq!(refused["error"]["code"], "preset_required");
        let resolved = super::super::presets::resolve(
            &case.fixture.app,
            &caller,
            &json!({
            "name":"Native", "prompt":"test", "harness":"gemini-cli", "access":"writer", "preset":id
            }),
        )
        .unwrap()
        .unwrap();
        let mut request = branchyard_client::api::TaskRequest {
            prompt: "test".into(),
            harness: Some("gemini-cli".into()),
            provider: Some(resolved.provider.clone()),
            ..Default::default()
        };
        let accepted =
            crate::work::preset_task_options(&case.fixture.app, &repo, &request, &resolved)
                .unwrap();
        assert_eq!(accepted.provider, request.provider);
        assert_eq!(
            crate::work::task_options(&case.fixture.app, &repo, &request)
                .err()
                .unwrap()
                .body
                .code,
            "preset_required"
        );
        request.provider = Some(serde_json::from_value(json!({"kind":"local"})).unwrap());
        assert_eq!(
            crate::work::preset_task_options(&case.fixture.app, &repo, &request, &resolved)
                .err()
                .unwrap()
                .body
                .code,
            "preset_conflict"
        );
    }
    assert!(case.sessions(&wid).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_graph_edits_cannot_advance_native_provider_work() {
    let case = Case::build(
        |config| {
            config.allow_delegation = true;
            config.by_path = Some("/bin/true".into());
            config
                .allow_providers
                .extend(["container".into(), "agent_sandbox".into()]);
        },
        true,
        &[],
    );
    let request = branchyard_client::api::TaskRequest {
        prompt: "Hello".into(),
        harness: Some("gemini-cli".into()),
        name: Some("native-parent".into()),
        delegation: Some(branchyard::Envelope::default()),
        // A home of its own, where Gemini CLI keeps the default policy.
        isolated: true,
        ..Default::default()
    };
    let (status, operation) = case
        .fixture
        .json(
            Method::POST,
            "/v1/repos/code/tasks",
            OWNER,
            Some("graph-parent"),
            serde_json::to_value(request).unwrap(),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{operation}");
    let finished = case.fixture.finished(operation["id"].as_str().unwrap());
    assert_eq!(finished.state, OperationState::Succeeded, "{finished:?}");
    let yard = &case.fixture.app.repos["code"].yard;
    let db = rusqlite::Connection::open(yard.root().join(".branchyard/state.db")).unwrap();
    for provider in [
        json!({"kind":"container","image":format!("operator/worker@sha256:{}", "a".repeat(64))}),
        json!({"kind":"agent_sandbox","context":"private-context","namespace":"branchyard-gvisor","image":format!("operator/worker@sha256:{}", "a".repeat(64))}),
    ] {
        // Craft a recorded target without contacting a runtime. Raw API edits
        // must reject it before graph admission, including non-spawn edits.
        let baseline = serde_json::from_value::<branchyard::Provider>(provider.clone()).unwrap();
        assert_eq!(db.execute("UPDATE branches SET record=json_set(record,'$.provider',json(?1)) WHERE name='native-parent'", [provider.to_string()]).unwrap(), 1);
        assert_eq!(
            yard.branch("native-parent").unwrap().provider().unwrap(),
            Some(baseline.clone())
        );
        for edits in [
            json!([{"kind":"spawn","prompt":"Hello","name":"must-not-start"}]),
            json!([]),
        ] {
            let (status, refused) = case
                .fixture
                .json(
                    Method::POST,
                    "/v1/repos/code/branches/native-parent/graph",
                    OWNER,
                    None,
                    json!({"expected_revision":0,"edits":edits}),
                )
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
            assert_eq!(refused["error"]["code"], "preset_required");
            assert!(yard.branch("must-not-start").is_err());
            assert_eq!(yard.graph("native-parent").unwrap().revision, 0);
            assert_eq!(
                yard.branch("native-parent").unwrap().provider().unwrap(),
                Some(baseline.clone())
            );
        }
        for route in ["send", "fork", "reincarnate", "spawn"] {
            let body = match route {
                "send" => json!({"prompt":"Hello"}),
                "reincarnate" => json!({"name":"must-not-start"}),
                _ => json!({"prompt":"Hello","name":"must-not-start"}),
            };
            let (status, refused) = case
                .fixture
                .json(
                    Method::POST,
                    &format!("/v1/repos/code/branches/native-parent/{route}"),
                    OWNER,
                    Some(&format!("raw-{route}-{}", provider["kind"])),
                    body,
                )
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{route}: {refused}");
            assert_eq!(
                refused["error"]["code"], "preset_required",
                "{route}: {refused}"
            );
            assert!(yard.branch("must-not-start").is_err());
            assert_eq!(
                yard.branch("native-parent").unwrap().provider().unwrap(),
                Some(baseline.clone())
            );
        }
    }
}

fn ids(list: &Value) -> Vec<&str> {
    list["presets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|preset| preset["id"].as_str().unwrap())
        .collect()
}

fn preset<'a>(list: &'a Value, id: &str) -> &'a Value {
    list["presets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|preset| preset["id"] == id)
        .unwrap()
}

fn harness<'a>(preset: &'a Value, id: &str) -> &'a Value {
    preset["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|harness| harness["harness"] == id)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_offers_tenant_presets_with_fixed_readiness_and_contacts_nothing() {
    let cluster = branchyard_testkit::MockHttp::start(|_| branchyard_testkit::Response::hang());
    let url = cluster.url();
    let mut secret_paths = Vec::new();
    let case = Case::new(|config| {
        let conf = config.data_dir.parent().unwrap().join("conf");
        std::fs::create_dir_all(&conf).unwrap();
        let live = conf.join("live-bridge.key");
        branchyard_bridge::Signer::write(&live).unwrap();
        let missing = conf.join("missing-bridge.key");
        secret_paths.extend([live.display().to_string(), missing.display().to_string()]);
        config.allow_providers.insert("substrate".into());
        config.allow_unapproved_tools = true;
        config.secrets.insert(
            "GEMINI_API_KEY".into(),
            crate::config::parse_secret("GEMINI_API_KEY").unwrap(),
        );
        let substrate = |key: &std::path::Path| json!({"kind":"substrate","endpoint":url,"router":url,"template":"harnesses","key":key});
        config.execution_presets = presets(json!([
            {"id":"local","label":"This server","provider":{"kind":"local"},
             "harnesses":[{"harness":"gemini-cli","models":["fast","deep"]},{"harness":"antigravity-stream-json"}],
             "secrets":["GEMINI_API_KEY"]},
            {"id":"substrate-gke","label":"Substrate on GKE","provider":substrate(&missing),
             "harnesses":[{"harness":"antigravity-stream-json","models":["flash"]}],"secrets":["GEMINI_API_KEY"],
             "tenants":["default"],"require_labels":["substrate-gke"]},
            {"id":"substrate-ready","label":"Substrate, keys present","provider":substrate(&live),
             "harnesses":[{"harness":"antigravity-stream-json"}]},
            {"id":"foreign-only","label":"Foreign","provider":{"kind":"local"},
             "harnesses":[{"harness":"gemini-cli"}],"tenants":["foreign"]}
        ]));
        config.validate().unwrap();
    });
    let (status, list) = case.presets(OWNER, None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let typed: ExecutionPresetList = serde_json::from_value(list.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&typed).unwrap(),
        list,
        "the client type reads every field"
    );
    assert_eq!(list["required"], false);
    assert_eq!(ids(&list), ["local", "substrate-gke", "substrate-ready"]);
    for offered in list["presets"].as_array().unwrap() {
        assert!(offered.get("can_start").is_none(), "{offered}");
        assert_eq!(offered["origin"], "operator");
    }
    let local = preset(&list, "local");
    assert_eq!(local["provider"], "local");
    assert_eq!(
        (
            &local["environment"]["state"],
            &local["environment"]["scope"]
        ),
        (&json!("ready"), &json!("api_host"))
    );
    assert!(local.get("reachability").is_none());
    let gemini = harness(local, "gemini-cli");
    assert_eq!(gemini["models"], json!(["fast", "deep"]));
    assert_eq!(gemini["default_model"], "fast");
    assert_eq!(gemini["executable"]["state"], "ready", "{gemini}");
    assert_eq!(gemini["executable"]["scope"], "api_host");
    assert!(gemini["executable"]["checked_ms_ago"].is_u64());
    let agy = harness(local, "antigravity-stream-json");
    assert_eq!(agy["tool_approvals"], false);
    assert_eq!(agy["writer"], true, "the operator allows unapproved tools");
    assert_eq!(agy["reviewer"], false);
    assert_eq!(agy["effort"], false);
    assert_eq!(agy["credentials"], "configured");
    assert_eq!(agy["auth"]["state"], "unknown");
    let gke = preset(&list, "substrate-gke");
    assert_eq!(gke["provider"], "substrate");
    assert_eq!(gke["environment"]["state"], "not_ready");
    assert_eq!(gke["environment"]["scope"], "api_host");
    assert!(gke["environment"]["checked_ms_ago"].is_u64());
    assert!(gke["environment"]["action"]
        .as_str()
        .unwrap()
        .contains("by serve --check"));
    assert_eq!(
        gke["reachability"],
        json!({"state":"unknown","scope":"environment",
            "reason":"The cluster is contacted when a session starts, not before.","action":null})
    );
    let remote = harness(gke, "antigravity-stream-json");
    assert_eq!(remote["default_model"], "flash");
    assert_eq!(
        (
            &remote["executable"]["state"],
            &remote["executable"]["scope"]
        ),
        (&json!("unknown"), &json!("environment"))
    );
    assert_eq!(
        preset(&list, "substrate-ready")["environment"]["state"],
        "ready"
    );
    assert_eq!(
        harness(preset(&list, "substrate-ready"), "antigravity-stream-json")["credentials"],
        "none"
    );
    let rendered = list.to_string();
    for configured in secret_paths.iter().chain([&url]) {
        assert!(
            !rendered.contains(configured.as_str()),
            "{configured} in {rendered}"
        );
    }
    let (status, foreign) = case.presets(FOREIGN, None).await;
    assert_eq!(status, StatusCode::OK, "{foreign}");
    assert_eq!(
        ids(&foreign),
        ["local", "substrate-ready", "foreign-only"],
        "a preset without tenants is every tenant's"
    );

    let wid = case.fixture.workspace().await;
    let (status, owned) = case.presets(OWNER, Some(&wid)).await;
    assert_eq!(status, StatusCode::OK, "{owned}");
    assert_eq!(preset(&owned, "local")["can_start"], true);
    assert_eq!(preset(&owned, "substrate-gke")["can_start"], true);
    for outsider in [EDITOR, FOREIGN] {
        let (status, refused) = case.presets(outsider, Some(&wid)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{refused}");
    }
    case.grant(&wid, "editor", "reviewer").await;
    let (_, reviewing) = case.presets(EDITOR, Some(&wid)).await;
    assert_eq!(
        preset(&reviewing, "local")["can_start"],
        true,
        "gemini-cli routes approvals, so a reviewer can start it"
    );
    assert_eq!(
        preset(&reviewing, "substrate-gke")["can_start"],
        false,
        "the Antigravity stream profile cannot enforce a reviewer's read-only policy"
    );
    case.grant(&wid, "tokenonly", "viewer").await;
    let (_, viewing) = case.presets(TOKENONLY, Some(&wid)).await;
    for offered in viewing["presets"].as_array().unwrap() {
        assert_eq!(offered["can_start"], false, "{offered}");
    }
    assert!(
        cluster.requests().is_empty(),
        "discovery contacted the environment"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_implicit_local_preset_exists_only_without_configured_presets() {
    let plain = Case::new(|_| {});
    let (status, list) = plain.presets(OWNER, None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(ids(&list), ["local"]);
    let local = preset(&list, "local");
    assert_eq!(local["origin"], "default");
    assert_eq!(local["provider"], "local");
    let gemini = harness(local, "gemini-cli");
    assert_eq!(gemini["executable"]["state"], "ready");
    assert_eq!(gemini["credentials"], "none");
    for offered in local["harnesses"].as_array().unwrap() {
        assert_eq!(offered["executable"]["state"], "ready", "{offered}");
    }
    let wid = plain.fixture.workspace().await;
    let (status, started) = plain
        .start(&wid, "implicit", json!({"preset":"local"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    let done = plain.fixture.finished(started["id"].as_str().unwrap());
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let session = started["session_id"].as_str().unwrap();
    let branch = plain.branch(&wid, session).unwrap();
    assert_eq!(branch.provider().unwrap().unwrap().label(), "local");

    let filtered = Case::new(|config| {
        config.execution_presets = presets(json!([{"id":"foreign-only","label":"Foreign",
            "provider":{"kind":"local"},"harnesses":[{"harness":"gemini-cli"}],"tenants":["foreign"]}]));
    });
    let (status, empty) = filtered.presets(OWNER, None).await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_eq!(empty, json!({"required":false,"presets":[]}));
    let wid = filtered.fixture.workspace().await;
    let (status, refused) = filtered
        .start(&wid, "implicit", json!({"preset":"local"}))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{refused}");
    assert_eq!(refused["error"]["code"], "unknown_preset");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn required_presets_refuse_raw_starts_and_continuations_keep_their_labels() {
    let case = Case::build(
        |config| {
            config.require_workspace_execution_presets = true;
            config.execution_presets = presets(json!([
                {"id":"pool","label":"Labelled pool","provider":{"kind":"local"},
                 "harnesses":[{"harness":"gemini-cli"}],"require_labels":["preset-pool"]},
                {"id":"foreign-only","label":"Foreign","provider":{"kind":"local"},
                 "harnesses":[{"harness":"gemini-cli"}],"tenants":["foreign"]}
            ]));
            config.validate().unwrap();
        },
        false,
        &["preset-pool"],
    );
    let (_, list) = case.presets(OWNER, None).await;
    assert_eq!(list["required"], true);
    let wid = case.fixture.workspace().await;
    for (key, fields, status, code, detail) in [
        ("raw", json!({}), 400, "preset_required", None),
        (
            "raw-provider",
            json!({"provider":{"kind":"local"}}),
            400,
            "preset_required",
            None,
        ),
        (
            "unknown",
            json!({"preset":"nope"}),
            404,
            "unknown_preset",
            None,
        ),
        (
            "other-tenant",
            json!({"preset":"foreign-only"}),
            404,
            "unknown_preset",
            None,
        ),
        (
            "with-provider",
            json!({"preset":"pool","provider":{"kind":"local"}}),
            400,
            "preset_conflict",
            None,
        ),
        (
            "with-provision-model",
            json!({"preset":"pool","provision":{"model":"pro"}}),
            400,
            "preset_conflict",
            None,
        ),
        (
            "other-harness",
            json!({"preset":"pool","harness":"codex"}),
            422,
            "preset_choice_unavailable",
            Some(json!({"field":"harness","allowed":["gemini-cli"]})),
        ),
        (
            "a-model",
            json!({"preset":"pool","model":"pro"}),
            422,
            "preset_choice_unavailable",
            Some(json!({"field":"model","allowed":[]})),
        ),
        (
            "an-effort",
            json!({"preset":"pool","effort":"high"}),
            422,
            "preset_choice_unavailable",
            Some(json!({"field":"effort","allowed":[]})),
        ),
    ] {
        let (got, refused) = case.start(&wid, key, fields).await;
        assert_eq!(got.as_u16(), status, "{key}: {refused}");
        assert_eq!(refused["error"]["code"], code, "{key}: {refused}");
        if let Some(detail) = detail {
            assert_eq!(refused["error"]["detail"], detail, "{key}");
        }
    }
    assert!(
        case.sessions(&wid).await.is_empty(),
        "refused starts left no session"
    );
    let (status, started) = case.start(&wid, "pool", json!({"preset":"pool"})).await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert!(started["requires"]
        .as_array()
        .unwrap()
        .contains(&json!("preset-pool")));
    let done = case.fixture.finished(started["id"].as_str().unwrap());
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let (status, replayed) = case.start(&wid, "pool", json!({"preset":"pool"})).await;
    assert_eq!(status, StatusCode::OK, "{replayed}");
    assert_eq!(replayed["id"], started["id"]);
    let sid = started["session_id"].as_str().unwrap();
    let sessions = case.sessions(&wid).await;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["preset"], "pool");
    assert!(
        sessions[0].get("_preset").is_none(),
        "the resolved environment stays on the server"
    );
    assert_eq!(
        case.branch(&wid, sid)
            .unwrap()
            .provider()
            .unwrap()
            .unwrap()
            .label(),
        "local"
    );
    let (status, sent) = case
        .fixture
        .json(
            Method::POST,
            &format!("/v1/workspaces/{wid}/sessions/{sid}/messages"),
            OWNER,
            Some("pool-send"),
            json!({"prompt":"Continue with the owners"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{sent}");
    assert!(
        sent["requires"]
            .as_array()
            .unwrap()
            .contains(&json!("preset-pool")),
        "a continuation requires the preset's labels: {sent}"
    );
    let done = case.fixture.finished(sent["id"].as_str().unwrap());
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    assert_eq!(
        case.branch(&wid, sid)
            .unwrap()
            .provider()
            .unwrap()
            .unwrap()
            .label(),
        "local"
    );
}

/// Two Substrate presets loaded from a configuration file, so their key is
/// written relative and resolved: one whose bridge key is missing, one whose
/// key is present and whose cluster is a closed port. Returns the
/// configuration directory.
fn unreachable_substrate(config: &mut Config) -> std::path::PathBuf {
    let conf = config.data_dir.parent().unwrap().join("conf");
    std::fs::create_dir_all(conf.join("substrate")).unwrap();
    std::fs::create_dir_all(conf.join("secrets")).unwrap();
    branchyard_bridge::Signer::write(&conf.join("substrate/live.key")).unwrap();
    std::fs::write(conf.join("secrets/gemini.key"), "not-a-real-key\n").unwrap();
    let file = conf.join("server.json");
    let closed = "http://127.0.0.1:1";
    let substrate = |key: &str| json!({"kind":"substrate","endpoint":closed,"router":closed,"template":"harnesses","key":key});
    std::fs::write(
        &file,
        json!({
            "data_dir": "state",
            "managed_knowledge": {"enabled": true},
            "tokens": [{"name": "operator", "token": "execution-preset-loader-token"}],
            "allow_providers": ["substrate"],
            "secrets": {"GEMINI_API_KEY": "@secrets/gemini.key"},
            "execution_presets": [
                {"id": "substrate-gke", "label": "Substrate on GKE",
                 "provider": substrate("substrate/missing.key"),
                 "harnesses": [{"harness": "gemini-cli"}], "secrets": ["GEMINI_API_KEY"]},
                {"id": "substrate-closed", "label": "Substrate, cluster closed",
                 "provider": substrate("substrate/live.key"),
                 "harnesses": [{"harness": "gemini-cli"}], "secrets": ["GEMINI_API_KEY"]}
            ]
        })
        .to_string(),
    )
    .unwrap();
    let loaded = crate::cli::resolve(&["--config".into(), file.display().to_string()]).unwrap();
    loaded.validate().unwrap();
    config.allow_providers.insert("substrate".into());
    config.secrets.extend(loaded.secrets);
    config.execution_presets = loaded.execution_presets;
    conf
}

/// What [`unreachable_substrate`] configured that no member may read of a
/// preset session.
fn configured(conf: &std::path::Path) -> [String; 6] {
    [
        conf.display().to_string(),
        "substrate/missing.key".into(),
        "substrate/live.key".into(),
        "127.0.0.1:1".into(),
        "GEMINI_API_KEY".into(),
        "gemini.key".into(),
    ]
}

fn assert_hidden(conf: &std::path::Path, what: &str, shown: &str) {
    for value in configured(conf) {
        assert!(!shown.contains(&value), "{what}: {value} shown in {shown}");
    }
}

/// Start a session on each unreachable preset; neither runs locally, and
/// what members read of its failure names no configured value, showing the
/// expected placeholder where the failure's text reaches them. Returns the
/// sessions' IDs.
async fn remote_failures_never_run_locally_and_are_projected(
    case: &Case,
    wid: &str,
    conf: &std::path::Path,
    expected: [(&str, Option<&str>); 2],
) -> Vec<String> {
    let (_, list) = case.presets(OWNER, Some(wid)).await;
    assert_eq!(
        preset(&list, "substrate-gke")["environment"]["state"],
        "not_ready",
        "the missing key shows, and does not refuse the start below"
    );
    let mut sessions = Vec::new();
    for (id, placeholder) in expected {
        let (status, started) = case
            .start(
                wid,
                id,
                json!({"preset":id,"prompt":"WRITE fallback.txt=ran locally"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{id}: {started}");
        let operation_id = started["id"].as_str().unwrap();
        let sid = started["session_id"].as_str().unwrap();
        let done = case.fixture.finished(operation_id);
        let (status, operation) = case
            .fixture
            .json(
                Method::GET,
                &format!("/v1/operations/{operation_id}"),
                OWNER,
                None,
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{operation}");
        let session = case
            .sessions(wid)
            .await
            .into_iter()
            .find(|session| session["id"] == sid)
            .unwrap();
        assert_eq!(session["preset"], id);
        let (_, messages) = case
            .fixture
            .json(
                Method::GET,
                &format!("/v1/workspaces/{wid}/sessions/{sid}/messages"),
                OWNER,
                None,
                Value::Null,
            )
            .await;
        let answered = messages["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|message| message["role"] == "assistant");
        let mut shown = format!("{operation}\n{session}\n{messages}");
        for gateway in ["runtime", "pr"] {
            let (_, read) = case
                .fixture
                .json(
                    Method::GET,
                    &format!("/v1/workspaces/{wid}/sessions/{sid}/{gateway}"),
                    OWNER,
                    None,
                    Value::Null,
                )
                .await;
            shown.push_str(&format!("\n{read}"));
        }
        assert_hidden(conf, id, &shown);
        if let Some(placeholder) = placeholder {
            assert!(
                shown.contains(placeholder),
                "{id}: {placeholder} not in {shown}"
            );
        }
        if let Some(branch) = case.branch(wid, sid) {
            assert_eq!(branch.provider().unwrap().unwrap().label(), "substrate");
            assert!(
                !branch.info().worktree.join("fallback.txt").exists(),
                "{id} ran on the server"
            );
            assert!(matches!(
                branch.info().status,
                branchyard::BranchStatus::Failed { .. }
            ));
        } else {
            assert_eq!(done.state, OperationState::Failed, "{done:?}");
        }
        assert!(!answered, "{id}: a harness answered");
        sessions.push(sid.to_owned());
    }
    sessions
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_presets_never_fall_back_and_knowledge_errors_are_projected() {
    let mut conf = std::path::PathBuf::new();
    let case = Case::new(|config| conf = unreachable_substrate(config));
    let wid = case.fixture.workspace().await;
    remote_failures_never_run_locally_and_are_projected(
        &case,
        &wid,
        &conf,
        [
            // A managed workspace's operation errors already read as one fixed sentence.
            ("substrate-gke", None),
            (
                "substrate-closed",
                Some("[preset substrate-closed: control endpoint]"),
            ),
        ],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_presets_never_fall_back_and_repository_errors_are_projected() {
    let mut conf = std::path::PathBuf::new();
    let case = Case::build(|config| conf = unreachable_substrate(config), true, &[]);
    let wid = case.repository_workspace().await;
    let sessions = remote_failures_never_run_locally_and_are_projected(
        &case,
        &wid,
        &conf,
        [
            ("substrate-gke", Some("[preset substrate-gke: bridge key]")),
            (
                "substrate-closed",
                Some("[preset substrate-closed: control endpoint]"),
            ),
        ],
    )
    .await;
    legacy_reads_are_projected(&case, &wid, &conf, &sessions).await;
    sessions_without_a_preset_read_as_the_engine_serializes_them(&case, &wid, &conf).await;
}

/// What a repository workspace's viewer reads of its preset sessions on
/// the legacy routes names no configured value, and the closed cluster's
/// failure shows with its placeholder wherever its text reaches.
async fn legacy_reads_are_projected(
    case: &Case,
    wid: &str,
    conf: &std::path::Path,
    sessions: &[String],
) {
    case.grant(wid, "editor", "viewer").await;
    let failure = "[preset substrate-closed: control endpoint]";
    for sid in sessions {
        let exists = case.branch(wid, sid).is_some();
        for route in [
            "",
            "/events",
            "/event-page",
            "/inspection",
            "/children",
            "/graph",
        ] {
            let path = format!("/v1/repos/code/branches/{sid}{route}");
            let (status, body) = case.read(EDITOR, &path).await;
            assert_eq!(status.is_success(), exists, "{path}: {body}");
            assert_hidden(conf, &path, &body);
            if exists && !matches!(route, "/children" | "/graph") {
                assert!(body.contains(failure), "{path}: {failure} not in {body}");
            }
        }
    }
    let closed = sessions
        .iter()
        .find(|sid| case.branch(wid, sid).is_some())
        .expect("the closed cluster's session has a branch");
    let (status, waited) = case
        .fixture
        .json(
            Method::POST,
            "/v1/repos/code/wait",
            OWNER,
            None,
            json!({"branches":[closed],"timeout_seconds":0}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{waited}");
    let mut reads = vec![("wait".to_owned(), waited.to_string())];
    for path in [
        "/v1/repos/code/branches".to_owned(),
        "/v1/repos/code/task-records".to_owned(),
        format!("/v1/repos/code/task-records/{closed}"),
        "/v1/repos/code/operations".to_owned(),
    ] {
        let (status, body) = case.read(EDITOR, &path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        reads.push((path, body));
    }
    reads.push(("feed".to_owned(), case.feed(EDITOR).await));
    for (what, body) in &reads {
        assert_hidden(conf, what, body);
        if what != "/v1/repos/code/operations" {
            assert!(body.contains(failure), "{what}: {failure} not in {body}");
        }
    }

    let message = format!(
        "could not reach 127.0.0.1:1 with {}",
        conf.join("substrate/live.key").display()
    );
    let app = &case.fixture.app;
    let listed = crate::api::operation_list(
        app,
        vec![
            operation("code", &[closed.as_str()], &message),
            operation("code", &["unrelated"], &message),
        ],
    )
    .unwrap();
    let listed: Value = serde_json::from_slice(&body_of(listed).await).unwrap();
    let shown = listed["operations"][0]["error"]["message"]
        .as_str()
        .unwrap();
    assert_hidden(conf, "an unfinished operation", shown);
    assert!(shown.contains(failure), "{shown}");
    assert_eq!(
        listed["operations"][1]["error"]["message"], message,
        "an operation outside every preset session keeps its text"
    );
    let unrelated = vec![operation("code", &["unrelated"], &message)];
    assert_eq!(
        body_of(crate::api::operation_list(app, unrelated.clone()).unwrap()).await,
        serde_json::to_vec(&OperationList {
            operations: unrelated
        })
        .unwrap()
    );
}

/// In the same workspace, a session started on a raw Substrate provider
/// and a local one read exactly as the engine serializes them, raw
/// endpoint included.
async fn sessions_without_a_preset_read_as_the_engine_serializes_them(
    case: &Case,
    wid: &str,
    conf: &std::path::Path,
) {
    let raw = json!({"kind":"substrate","endpoint":"http://127.0.0.1:1","router":"http://127.0.0.1:1",
        "template":"harnesses","key":conf.join("substrate/live.key")});
    for (key, fields) in [
        ("raw-substrate", json!({"provider":raw})),
        ("local", json!({})),
    ] {
        let (status, started) = case.start(wid, key, fields).await;
        assert_eq!(status, StatusCode::OK, "{key}: {started}");
        let sid = started["session_id"].as_str().unwrap();
        case.fixture.finished(started["id"].as_str().unwrap());
        let native = case.branch(wid, sid).unwrap();
        let (status, info) = case
            .fixture
            .bytes(
                Method::GET,
                &format!("/v1/repos/code/branches/{sid}"),
                OWNER,
                None,
                &[],
                Vec::new(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{key}");
        assert_eq!(
            info,
            serde_json::to_vec(native.info()).unwrap(),
            "{key}: the branch as the engine serializes it"
        );
        let mut events = Vec::new();
        let mut cursor = 0;
        loop {
            let page = native.events_since(cursor, 500).unwrap();
            if page.events.is_empty() {
                break;
            }
            cursor = page.next_cursor;
            events.extend(page.events);
        }
        let (status, page) = case
            .fixture
            .bytes(
                Method::GET,
                &format!("/v1/repos/code/branches/{sid}/events"),
                OWNER,
                None,
                &[],
                Vec::new(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{key}");
        assert_eq!(
            page,
            serde_json::to_vec(&BranchEvents { events, cursor }).unwrap(),
            "{key}: the events as the engine serializes them"
        );
        if key == "raw-substrate" {
            let info = String::from_utf8(info).unwrap();
            assert!(info.contains("127.0.0.1:1"), "{info}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_branch_takes_the_projection_of_its_nearest_session_on_the_same_repository() {
    let case = Case::build(
        |config| {
            use std::os::unix::fs::PermissionsExt as _;
            config.allow_delegation = true;
            // Delegating branches need a `by` for their tools; CI has none on PATH,
            // and the fake agent never calls it.
            let by = config.data_dir.parent().unwrap().join("fake-by");
            std::fs::write(&by, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&by, std::fs::Permissions::from_mode(0o755)).unwrap();
            config.by_path = Some(by);
        },
        true,
        &[],
    );
    let f = &case.fixture;
    for (name, parent) in [
        ("root", None),
        ("middle", Some("root")),
        ("leaf", Some("middle")),
        ("other", None),
    ] {
        let mut body = json!({"prompt":"Summarise the backlog","harness":"gemini-cli","name":name});
        let path = match parent {
            None => {
                body["delegation"] = json!({"max_depth":3,"max_children":3,"harnesses":[]});
                // A home of its own, which its children share: there Gemini
                // CLI keeps the default policy.
                body["isolated"] = json!(true);
                "/v1/repos/code/tasks".to_owned()
            }
            Some(parent) => format!("/v1/repos/code/branches/{parent}/spawn"),
        };
        let (status, started) = f.json(Method::POST, &path, OWNER, Some(name), body).await;
        assert!(status.is_success(), "{name}: {started}");
        let done = f.finished(started["id"].as_str().unwrap());
        assert_eq!(done.state, OperationState::Succeeded, "{name}: {done:?}");
    }
    let preset = |id: &str, hidden: &[(&str, &str)]| {
        let hidden: Vec<Value> = hidden
            .iter()
            .map(|(value, placeholder)| json!({"value":value,"placeholder":placeholder}))
            .collect();
        json!({"id":id,"provider":{"kind":"local"},"secrets":[],"require_labels":[],
            "model":null,"hidden":hidden})
    };
    let text = "at /srv/conf/key.pem and /srv/conf";
    let near = "at [near]/key.pem and [near]";
    let shown = |branch: &str| {
        let mut value = json!(text);
        Projection::of_branch(&f.app, "code", branch)
            .unwrap()
            .apply(&mut value);
        value
    };
    let merged = |branches: &[&str]| {
        let mut value = json!(text);
        Projection::of_operation(&f.app, &operation("code", branches, ""))
            .unwrap()
            .apply(&mut value);
        value
    };
    put(
        &f.app,
        Collection::Workspaces,
        "on-code",
        json!({"repo":"code"}),
    );
    put(
        &f.app,
        Collection::Workspaces,
        "on-other",
        json!({"repo":"other"}),
    );
    session_row(
        &f.app,
        "root",
        "on-code",
        preset("near", &[("/srv/conf", "[near]")]),
    );
    for branch in ["root", "middle", "leaf"] {
        assert_eq!(shown(branch), near, "{branch} is under the root session");
    }
    assert_eq!(shown("other"), text);
    session_row(
        &f.app,
        "leaf",
        "on-other",
        preset("foreign", &[("/srv/conf", "[foreign]")]),
    );
    assert_eq!(
        shown("leaf"),
        near,
        "a session of the same name on another repository owns nothing here"
    );
    session_row(
        &f.app,
        "other",
        "on-code",
        preset(
            "far",
            &[
                ("/srv/conf", "[far conf]"),
                ("/srv/conf/key.pem", "[far key]"),
            ],
        ),
    );
    assert_eq!(
        merged(&["leaf", "other"]),
        "at [far key] and [near]",
        "longest first; a value both hide takes the first branch's placeholder"
    );
    assert_eq!(merged(&["other", "leaf"]), "at [far key] and [far conf]");
    assert_eq!(merged(&["unrelated"]), text);
    session_row(&f.app, "middle", "on-code", Value::Null);
    assert_eq!(shown("leaf"), text, "the nearest session has no preset");
    assert_eq!(shown("middle"), text);
    assert_eq!(shown("root"), near);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_middleware_projects_a_preset_sessions_errors_and_gateway_reads() {
    let mut conf = std::path::PathBuf::new();
    let case = Case::new(|config| conf = unreachable_substrate(config));
    let app = &case.fixture.app;
    let owner = Caller::new(Principal::default_for("owner"));
    let resolved = super::super::presets::resolve(
        app,
        &owner,
        &json!({"name":"Analyst","prompt":"Summarise","harness":"gemini-cli","access":"writer",
            "preset":"substrate-closed"}),
    )
    .unwrap()
    .unwrap();
    put(
        app,
        Collection::Workspaces,
        "on-code",
        json!({"repo":"code"}),
    );
    session_row(app, "s-preset", "on-code", json!(resolved));
    session_row(app, "s-raw", "on-code", Value::Null);
    session_row(app, "s-damaged", "on-code", json!("not a stored preset"));
    let key = conf.join("substrate/live.key").display().to_string();
    let message = format!("cannot reach http://127.0.0.1:1 with {key}");
    let error =
        json!({"error":{"code":"provider_unavailable","message":message,"detail":{"key":key}}});
    let respond = |status: StatusCode, body: &Value| {
        let mut response = (status, axum::Json(body.clone())).into_response();
        response
            .headers_mut()
            .insert("x-request-id", axum::http::HeaderValue::from_static("kept"));
        response
    };
    let redacted = |method: Method, path: String, response: axum::response::Response| async move {
        let response = super::super::auth::redact_response(app, &method, &path, response).await;
        let status = response.status();
        let length = response
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .cloned();
        assert_eq!(response.headers()["x-request-id"], "kept", "{path}");
        let body = body_of(response).await;
        if let Some(length) = length {
            assert_eq!(length, body.len().to_string(), "{path}");
        }
        (status, body)
    };
    let session = "/v1/workspaces/on-code/sessions";
    for (method, path, status) in [
        (
            Method::POST,
            format!("{session}/s-preset/messages"),
            StatusCode::CONFLICT,
        ),
        (
            Method::GET,
            format!("{session}/s-preset/runtime"),
            StatusCode::NOT_FOUND,
        ),
        (
            Method::POST,
            "/v1/repos/code/branches/s%2Dpreset/send".to_owned(),
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
    ] {
        let (got, body) = redacted(method, path.clone(), respond(status, &error)).await;
        assert_eq!(got, status, "{path}");
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "provider_unavailable", "{path}");
        assert_hidden(&conf, &path, &body.to_string());
        assert_eq!(
            body["error"]["message"],
            "cannot reach [preset substrate-closed: control endpoint] with [preset \
             substrate-closed: bridge key]",
            "{path}"
        );
    }
    let read = json!({"state":"unavailable","reason":message});
    for gateway in ["runtime", "pr"] {
        let path = format!("{session}/s-preset/{gateway}");
        let (status, body) =
            redacted(Method::GET, path.clone(), respond(StatusCode::OK, &read)).await;
        assert_eq!(status, StatusCode::OK);
        assert_hidden(&conf, &path, &String::from_utf8(body).unwrap());
    }
    let page = json!({"cursor":1,"data":message,"encoding":"base64","error":message});
    let (status, body) = redacted(
        Method::GET,
        format!("{session}/s-preset/terminals/t1/output"),
        respond(StatusCode::OK, &page),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["data"], message, "terminal bytes are never rewritten");
    assert_hidden(&conf, "terminal error", body["error"].as_str().unwrap());

    for (method, path, status, body) in [
        (
            Method::GET,
            format!("{session}/s-preset/messages"),
            StatusCode::OK,
            &read,
        ),
        (
            Method::GET,
            format!("{session}/s-raw/runtime"),
            StatusCode::OK,
            &read,
        ),
        (
            Method::POST,
            format!("{session}/s-raw/messages"),
            StatusCode::CONFLICT,
            &error,
        ),
        (
            Method::POST,
            "/v1/repos/code/branches/s-raw/send".to_owned(),
            StatusCode::CONFLICT,
            &error,
        ),
    ] {
        let (got, shown) = redacted(method, path.clone(), respond(status, body)).await;
        assert_eq!(got, status, "{path}");
        assert_eq!(
            shown,
            serde_json::to_vec(body).unwrap(),
            "{path} is unchanged"
        );
    }
    let mut plain = (StatusCode::CONFLICT, message.clone()).into_response();
    plain
        .headers_mut()
        .insert("x-request-id", axum::http::HeaderValue::from_static("kept"));
    let plain = super::super::auth::redact_response(
        app,
        &Method::POST,
        &format!("{session}/s-preset/messages"),
        plain,
    )
    .await;
    assert_eq!(
        body_of(plain).await,
        message.as_bytes(),
        "only JSON is rewritten"
    );

    for (method, path, status, withheld) in [
        (
            Method::POST,
            format!("{session}/s-damaged/messages"),
            StatusCode::CONFLICT,
            StatusCode::CONFLICT,
        ),
        (
            Method::GET,
            format!("{session}/s-damaged/runtime"),
            StatusCode::OK,
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        let (got, body) = redacted(method, path.clone(), respond(status, &error)).await;
        assert_eq!(got, withheld, "{path}");
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "workspace_error", "{path}: {body}");
        assert_hidden(&conf, &path, &body.to_string());
    }
}

/// `POST /v1/repos/{repo}/tasks` with `preset` (`by run --env`): resolved
/// for the caller at admission, its labels on the queue row, run on its
/// provider, and refused as a session start is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_tasks_run_on_a_preset_and_are_refused_as_session_starts_are() {
    let case = Case::build(
        |config| {
            config.execution_presets = presets(json!([
                {"id":"pool","label":"Labelled pool","provider":{"kind":"local"},
                 "harnesses":[{"harness":"gemini-cli","models":["flash"]}],"require_labels":["preset-pool"]},
                {"id":"foreign-only","label":"Foreign","provider":{"kind":"local"},
                 "harnesses":[{"harness":"gemini-cli"}],"tenants":["foreign"]}
            ]));
        },
        true,
        &["preset-pool"],
    );
    let task = |key: &'static str, fields: Value| {
        let mut body = json!({"prompt":"WRITE preset.txt=ran"});
        body.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let fixture = &case.fixture;
        async move {
            fixture
                .json(Method::POST, "/v1/repos/code/tasks", OWNER, Some(key), body)
                .await
        }
    };
    for (key, fields, status, code) in [
        ("unknown", json!({"preset":"nope"}), 404, "unknown_preset"),
        (
            "other-tenant",
            json!({"preset":"foreign-only"}),
            404,
            "unknown_preset",
        ),
        (
            "with-provider",
            json!({"preset":"pool","provider":{"kind":"local"}}),
            400,
            "preset_conflict",
        ),
        (
            "with-provision",
            json!({"preset":"pool","provision":{"model":"pro"}}),
            400,
            "preset_conflict",
        ),
        (
            "with-command",
            json!({"preset":"pool","command":["sh"]}),
            400,
            "preset_conflict",
        ),
        (
            "other-harness",
            json!({"preset":"pool","harness":"codex"}),
            422,
            "preset_choice_unavailable",
        ),
    ] {
        let (got, refused) = task(key, fields).await;
        assert_eq!(got.as_u16(), status, "{key}: {refused}");
        assert_eq!(refused["error"]["code"], code, "{key}: {refused}");
    }

    // No harness named: the preset's first runs, on the worker that
    // carries its label, with its default model.
    let (status, admitted) = task("ok", json!({"preset":"pool"})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{admitted}");
    let done = case.fixture.finished(admitted["id"].as_str().unwrap());
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let name = done.branches.first().cloned().unwrap();
    let repo = case.fixture.app.repos["code"].yard.clone();
    let branch = repo.branch(&name).unwrap();
    assert_eq!(branch.info().harness, "gemini-cli");
    assert!(branch.info().worktree.join("preset.txt").exists());
    assert_eq!(
        done.requires,
        ["preset-pool"],
        "the preset's label is on the queue row"
    );
    let row = case
        .fixture
        .app
        .registry
        .get(done.id.as_str())
        .unwrap()
        .unwrap();
    assert_eq!(row.state, OperationState::Succeeded);
}

/// The resolution itself: the preset's provider, secrets, labels and
/// model; another tenant cannot name it; and a request with a preset is
/// refused by every path that does not resolve it for a tenant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_preset_resolves_per_tenant_and_nowhere_else() {
    let image = format!("private-registry/worker@sha256:{}", "a".repeat(64));
    let case = Case::build(
        |config| {
            config.allow_providers.insert("agent_sandbox".into());
            config.secrets.insert(
                "GEMINI_API_KEY".into(),
                branchyard::SecretSource::parse("GEMINI_API_KEY=GEMINI_API_KEY").unwrap(),
            );
            config.execution_presets = presets(json!([
                {"id":"gke-kata","label":"GKE Kata",
                 "provider":{"kind":"agent_sandbox","context":"private-context","namespace":"branchyard-microvm",
                     "runtime":"microvm","image":image},
                 "harnesses":[{"harness":"gemini-cli","models":["flash","pro"]},{"harness":"codex"}],
                 "secrets":["GEMINI_API_KEY"],"require_labels":["compute:gke-kata"],"tenants":["default"]}
            ]));
        },
        true,
        &[],
    );
    let app = &case.fixture.app;
    let request = branchyard_client::api::TaskRequest {
        prompt: "test".into(),
        preset: Some("gke-kata".into()),
        require_labels: vec!["linux".into()],
        ..Default::default()
    };
    let (resolved, preset) = crate::console::for_task(app, "default", &request).unwrap();
    assert_eq!(resolved.preset, None);
    assert!(
        resolved.isolated,
        "a preset's secrets go into a private home"
    );
    assert_eq!(resolved.harness.as_deref(), Some("gemini-cli"));
    assert_eq!(resolved.provider.as_ref().unwrap().name(), "agent_sandbox");
    assert_eq!(resolved.require_labels, ["linux", "compute:gke-kata"]);
    let provision = resolved.provision.as_ref().unwrap();
    assert_eq!(provision.model.as_deref(), Some("flash"));
    assert_eq!(provision.secrets[0].name, "GEMINI_API_KEY");
    assert_eq!(
        preset.provider.compute().isolation,
        branchyard::Isolation::Kata
    );

    let fan = branchyard_client::api::TaskRequest {
        harnesses: vec!["gemini-cli".into(), "codex".into()],
        ..request.clone()
    };
    let (resolved, _) = crate::console::for_task(app, "default", &fan).unwrap();
    assert_eq!(
        resolved.provision.unwrap().model,
        None,
        "one provisioning, several harnesses"
    );

    let refused = crate::console::for_task(app, "foreign", &request)
        .err()
        .unwrap();
    assert_eq!(
        refused.body.code, "unknown_preset",
        "the operator narrowed it"
    );
    let repo = &app.repos["code"];
    let unresolved = crate::work::task_options(app, repo, &request)
        .err()
        .unwrap();
    assert_eq!(
        unresolved.body.code, "invalid_request",
        "{:?}",
        unresolved.body
    );
    let without_tenant = crate::work::preset_or_task_options(app, repo, &request, None)
        .err()
        .unwrap();
    assert_eq!(without_tenant.body.code, "invalid_request");
    let (_, options, _) =
        crate::work::preset_or_task_options(app, repo, &request, Some("default")).unwrap();
    assert_eq!(options.provider.unwrap().name(), "agent_sandbox");
}

/// Direct tasks use the session projection at admission, during execution,
/// and when a completed operation is fetched or retried.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_preset_task_failures_are_projected_at_admission_execution_and_replay() {
    let mut conf = std::path::PathBuf::new();
    let case = Case::build(|config| conf = unreachable_substrate(config), true, &[]);
    let mut closed_operation = None;
    for (preset, marker) in [
        ("substrate-gke", "bridge key"),
        ("substrate-closed", "control endpoint"),
    ] {
        let body = json!({"preset":preset,"name":format!("direct-{preset}"),
            "prompt":"WRITE fallback.txt=ran locally"});
        let (status, admitted) = case
            .fixture
            .json(
                Method::POST,
                "/v1/repos/code/tasks",
                OWNER,
                Some(preset),
                body.clone(),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{admitted}");
        assert_hidden(&conf, "admission", &admitted.to_string());
        let id = admitted["id"].as_str().unwrap();
        let done = case.fixture.finished(id);
        if preset == "substrate-closed" {
            closed_operation = Some(done.clone());
        }
        let (status, shown) = case
            .fixture
            .json(
                Method::GET,
                &format!("/v1/operations/{id}"),
                OWNER,
                None,
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{shown}");
        assert_hidden(&conf, "completed operation", &shown.to_string());
        assert!(
            shown
                .to_string()
                .contains(&format!("[preset {preset}: {marker}]")),
            "{shown}"
        );
        if let Some(error) = &done.error {
            assert_eq!(shown["error"]["code"], error.code);
        }
        let (status, replayed) = case
            .fixture
            .json(
                Method::POST,
                "/v1/repos/code/tasks",
                OWNER,
                Some(preset),
                body,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{replayed}");
        assert_eq!(replayed["id"], id);
        assert_eq!(replayed, shown);
        for name in &done.branches {
            if let Ok(branch) = case.fixture.app.repos["code"].yard.branch(name) {
                assert!(matches!(
                    branch.info().status,
                    branchyard::BranchStatus::Failed { .. }
                ));
                assert!(!branch.info().worktree.join("fallback.txt").exists());
                for suffix in ["", "/events", "/event-page", "/inspection"] {
                    let path = format!("/v1/repos/code/branches/{name}{suffix}");
                    let (status, value) = case.read(OWNER, &path).await;
                    assert_eq!(status, StatusCode::OK, "{path}: {value}");
                    assert_hidden(&conf, &path, &value);
                }
            }
        }
    }
    for path in ["/v1/repos/code/branches", "/v1/repos/code/task-records"] {
        let (status, value) = case.read(OWNER, path).await;
        assert_eq!(status, StatusCode::OK, "{path}: {value}");
        assert_hidden(&conf, path, &value);
    }
    assert_hidden(&conf, "feed", &case.feed(OWNER).await);

    // Worktree paths and names are reusable; a new raw task with the same
    // name must not inherit the removed task's projection.
    let name = "direct-substrate-closed";
    let yard = &case.fixture.app.repos["code"].yard;
    let native_task = branchyard::tasks::task_of(yard.root(), name)
        .unwrap()
        .unwrap();
    let child = "historical-child";
    let child_request = branchyard_client::api::TaskRequest {
        prompt: "WRITE child.txt=ran".into(),
        name: Some(child.into()),
        harness: Some("gemini-cli".into()),
        isolated: true,
        ..Default::default()
    };
    let mut child_options = crate::work::task_options(
        &case.fixture.app,
        &case.fixture.app.repos["code"],
        &child_request,
    )
    .unwrap();
    child_options.join_task = Some(native_task.id);
    yard.task("WRITE child.txt=ran")
        .options(child_options)
        .run()
        .unwrap();
    yard.remove(child).unwrap();
    case.fixture.app.repos["code"].yard.remove(name).unwrap();
    let old = closed_operation.unwrap();
    let history =
        Projection::of_feed_entry(&case.fixture.app, "code", child, old.end_cursor.unwrap())
            .unwrap();
    assert!(
        !history.is_empty(),
        "removed attempts retain their task's projection"
    );
    let (status, raw) = case
        .fixture
        .json(
            Method::POST,
            "/v1/repos/code/tasks",
            OWNER,
            Some("reuse-raw-name"),
            json!({"prompt":"WRITE raw.txt=ran", "name":name,"harness":"gemini-cli","isolated":true}),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{raw}");
    let done = case.fixture.finished(raw["id"].as_str().unwrap());
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    assert!(Projection::of_branch(&case.fixture.app, "code", name)
        .unwrap()
        .is_empty());
    assert_hidden(
        &conf,
        "historical feed after branch name reuse",
        &case.feed(OWNER).await,
    );
    // The public raw-task flow keeps its own diagnostics. Only a resolved
    // tenant-offered preset establishes this projection boundary.
    let (status, refused) = case
        .fixture
        .json(
            Method::POST,
            "/v1/repos/code/tasks",
            OWNER,
            Some("raw-marker"),
            json!({"prompt":"unused", "name":"substrate/missing.key"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert!(refused["error"]["message"]
        .as_str()
        .unwrap()
        .contains("substrate/missing.key"));

    let (status, refused) = case
        .fixture
        .json(
            Method::POST,
            "/v1/repos/code/tasks",
            OWNER,
            Some("preset-invalid-name"),
            json!({"prompt":"unused", "preset":"substrate-gke", "name":"substrate/missing.key"}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["error"]["code"], "invalid_name");
    assert_hidden(&conf, "planning error", &refused.to_string());
}
