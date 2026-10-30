//! Execution presets through the real routes, queue and native engine, with
//! the fake agent and Substrate environments that are never reached.

use super::*;
use crate::config::ExecutionPreset;
use branchyard_client::api::OperationState;
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

fn presets(value: Value) -> Vec<ExecutionPreset> {
    serde_json::from_value(value).unwrap()
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
             "harnesses":[{"harness":"gemini-cli","models":["fast","deep"]},{"harness":"antigravity"}],
             "secrets":["GEMINI_API_KEY"]},
            {"id":"substrate-gke","label":"Substrate on GKE","provider":substrate(&missing),
             "harnesses":[{"harness":"antigravity","models":["flash"]}],"secrets":["GEMINI_API_KEY"],
             "tenants":["default"],"require_labels":["substrate-gke"]},
            {"id":"substrate-ready","label":"Substrate, keys present","provider":substrate(&live),
             "harnesses":[{"harness":"antigravity"}]},
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
    let agy = harness(local, "antigravity");
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
    let remote = harness(gke, "antigravity");
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
        harness(preset(&list, "substrate-ready"), "antigravity")["credentials"],
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
        "antigravity cannot enforce a reviewer's read-only policy"
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

/// Start a session on each unreachable preset; neither runs locally, and
/// what members read of its failure names no configured value, showing the
/// expected placeholder where the failure's text reaches them.
async fn remote_failures_never_run_locally_and_are_projected(
    case: &Case,
    wid: &str,
    conf: &std::path::Path,
    expected: [(&str, Option<&str>); 2],
) {
    let (_, list) = case.presets(OWNER, Some(wid)).await;
    assert_eq!(
        preset(&list, "substrate-gke")["environment"]["state"],
        "not_ready",
        "the missing key shows, and does not refuse the start below"
    );
    let configured = [
        conf.display().to_string(),
        "substrate/missing.key".into(),
        "substrate/live.key".into(),
        "127.0.0.1:1".into(),
        "GEMINI_API_KEY".into(),
        "gemini.key".into(),
    ];
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
        let shown = format!("{operation}\n{session}\n{messages}");
        for value in &configured {
            assert!(
                !shown.contains(value.as_str()),
                "{id}: {value} shown in {shown}"
            );
        }
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
    }
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
    remote_failures_never_run_locally_and_are_projected(
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
}
