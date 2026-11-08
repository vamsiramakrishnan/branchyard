use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Method, Request};
use axum::Extension;
use serde_json::json;
use tower::ServiceExt as _;

use super::*;
use crate::api::{App, Caller};
use crate::config::Principal;

const IMAGE: &str =
    "ghcr.io/example/harnesses@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn preset(value: Value) -> ExecutionPreset {
    serde_json::from_value(value).unwrap()
}

fn worker(id: &str, labels: &[&str]) -> LiveWorker {
    LiveWorker {
        id: id.into(),
        host: format!("{id}.internal"),
        labels: labels.iter().map(|l| (*l).to_owned()).collect(),
        repos: vec!["app".into()],
        seen_ms_ago: 1200,
        inventory: None,
    }
}

fn configured() -> Config {
    let mut config = Config::new(PathBuf::from("/nonexistent/data"));
    config.allow_providers = ["agent_sandbox".to_owned()].into();
    config.execution_presets = vec![
        preset(
            json!({"id": "local", "label": "This server", "provider": {"kind": "local"},
            "harnesses": [{"harness": "claude-code"}]}),
        ),
        preset(json!({"id": "gke-kata", "label": "GKE Kata",
            "provider": {"kind": "agent_sandbox", "context": "ctx", "namespace": "branchyard-microvm",
                "image": IMAGE, "runtime": "microvm", "network": "deny"},
            "harnesses": [{"harness": "codex"}], "require_labels": ["compute:gke-kata"],
            "tenants": ["research"]})),
        preset(json!({"id": "gce-gvisor", "label": "GCE gVisor",
            "provider": {"kind": "container", "image": IMAGE, "runtime": "gvisor", "network": "unrestricted"},
            "harnesses": [{"harness": "claude-code"}], "require_labels": ["compute:gce-gvisor"]})),
    ];
    config
}

#[test]
fn each_preset_reads_as_compute_with_the_workers_that_can_claim_it() {
    let workers = [
        worker("w-gke", &["compute:gke-kata", "linux"]),
        worker("w-self", &[]),
    ];
    let report = compute(&configured(), &workers, "w-self");
    assert_eq!(report.origin, "configured");
    let by_id = |id: &str| report.targets.iter().find(|t| t.preset == id).unwrap();

    let local = by_id("local");
    assert_eq!(
        (local.platform, local.isolation),
        (Platform::Worker, Isolation::Process)
    );
    assert_eq!(local.state, "ready");
    assert_eq!(
        local.workers,
        ["w-gke", "w-self"],
        "no labels: every worker claims"
    );

    let kata = by_id("gke-kata");
    assert_eq!(
        (kata.platform, kata.isolation),
        (Platform::Gke, Isolation::Kata)
    );
    assert_eq!(kata.provider, "agent_sandbox");
    assert_eq!(kata.namespace.as_deref(), Some("branchyard-microvm"));
    assert_eq!(kata.network.as_deref(), Some("deny"));
    assert_eq!(kata.image.as_deref(), Some(IMAGE));
    assert_eq!(kata.tenants, Some(vec!["research".to_owned()]));
    assert_eq!(
        (kata.state, kata.workers.as_slice()),
        ("ready", &["w-gke".to_owned()][..])
    );

    let gvisor = by_id("gce-gvisor");
    assert_eq!(
        (gvisor.platform, gvisor.isolation),
        (Platform::Worker, Isolation::Gvisor)
    );
    assert_eq!(gvisor.network.as_deref(), Some("unrestricted"));
    assert_eq!(
        gvisor.state, "provider_not_allowed",
        "container is not in allow_providers"
    );
    assert!(!gvisor.allowed);

    assert_eq!(report.workers[0].id, "w-self", "this server comes first");
    assert!(report.workers[0].this);
    assert_eq!(report.workers[0].targets, ["local"]);
    assert_eq!(report.workers[1].targets, ["local", "gke-kata"]);
}

#[test]
fn an_allowed_preset_with_no_matching_worker_says_how_to_start_one() {
    let mut config = configured();
    config.allow_providers.insert("container".into());
    let report = compute(&config, &[worker("w-self", &["linux"])], "w-self");
    let gvisor = report
        .targets
        .iter()
        .find(|t| t.preset == "gce-gvisor")
        .unwrap();
    assert_eq!(gvisor.state, "no_worker");
    assert!(gvisor.next.contains("by worker --label"), "{}", gvisor.next);
}

#[test]
fn without_presets_the_implicit_local_preset_is_reported() {
    let config = Config::new(PathBuf::from("/nonexistent/data"));
    let report = compute(&config, &[], "w-self");
    assert_eq!(report.origin, "default");
    assert_eq!(report.targets.len(), 1);
    assert_eq!(report.targets[0].preset, "local");
    assert_eq!(report.targets[0].state, "no_worker");
}

fn request(value: Value) -> PlanRequest {
    serde_json::from_value(value).unwrap()
}

fn gke(runtime: &str) -> Value {
    json!({"id": "gke-pool", "platform": "gke", "runtime": runtime, "project": "my-project",
        "zone": "us-central1-a", "name": "by-sandbox", "version": "0.5.0", "image": IMAGE,
        "gke_version": "1.37.1-gke.3020000",
        "service_account": "nodes@my-project.iam.gserviceaccount.com",
        "master_cidr": "172.30.80.0/28"})
}

#[test]
fn a_gke_kata_plan_runs_the_helper_then_applies_and_names_the_microvm_pool() {
    let plan = plan(&request(gke("kata"))).unwrap();
    assert_eq!(plan.label, "compute:gke-pool");
    assert_eq!(plan.allow_provider, "agent_sandbox");
    assert_eq!(plan.steps.len(), 3);
    assert_eq!(
        plan.steps[0].argv,
        [
            "python3",
            "tools/agent_sandbox_gke.py",
            "--project=my-project",
            "--zone=us-central1-a",
            "--cluster=by-sandbox",
            "--version=1.37.1-gke.3020000",
            "--service-account=nodes@my-project.iam.gserviceaccount.com",
            "--master-cidr=172.30.80.0/28",
        ]
    );
    assert_eq!(
        plan.steps[1].argv.last().map(String::as_str),
        Some("--apply")
    );
    assert_eq!(plan.steps[2].on, "worker");
    assert_eq!(
        plan.steps[2].argv,
        ["by", "serve", "--label", "compute:gke-pool"]
    );
    assert_eq!(
        plan.preset["provider"],
        json!({"kind": "agent_sandbox", "context": "gke_my-project_us-central1-a_by-sandbox",
            "namespace": "branchyard-microvm", "image": IMAGE, "runtime": "microvm", "network": "deny"})
    );
    assert_eq!(plan.preset["require_labels"], json!(["compute:gke-pool"]));
    // The preset the plan prints is one the server's configuration accepts.
    let parsed = preset(plan.preset.clone());
    assert_eq!(parsed.require_labels, ["compute:gke-pool"]);
    assert_eq!(parsed.harnesses[0].harness, "claude-code");
}

#[test]
fn a_gke_gvisor_plan_names_the_gvisor_pool() {
    let plan = plan(&request(gke("gvisor"))).unwrap();
    assert_eq!(plan.preset["provider"]["namespace"], "branchyard-gvisor");
    assert_eq!(plan.preset["provider"]["runtime"], "gvisor");
}

#[test]
fn gce_plans_choose_the_runtime_flag_and_the_provider() {
    let base = json!({"id": "gce-1", "platform": "gce", "project": "my-project",
        "zone": "us-central1-a", "name": "by-worker", "version": "0.5.0", "image": IMAGE,
        "tunnel_through_iap": true, "harnesses": ["codex", "claude-code"]});
    let with = |runtime: &str| {
        let mut value = base.clone();
        value["runtime"] = json!(runtime);
        plan(&request(value)).unwrap()
    };
    let gvisor = with("gvisor");
    assert_eq!(
        gvisor.steps[0].argv,
        [
            "by",
            "compute",
            "gce",
            "--project=my-project",
            "--zone=us-central1-a",
            "--name=by-worker",
            "--version=0.5.0",
            "--gvisor",
            "--tunnel-through-iap",
        ]
    );
    assert_eq!(gvisor.allow_provider, "container");
    assert_eq!(
        gvisor.preset["provider"],
        json!({"kind": "container", "image": IMAGE, "runtime": "gvisor", "network": "deny"})
    );
    assert_eq!(
        gvisor.preset["harnesses"],
        json!([{"harness": "codex"}, {"harness": "claude-code"}])
    );
    assert_eq!(
        gvisor.steps[2].argv.last().map(String::as_str),
        Some("compute:gce-1")
    );

    let micro = with("microsandbox");
    assert!(micro.steps[0]
        .argv
        .contains(&"--backend=microsandbox".to_owned()));
    assert_eq!(
        micro.preset["provider"],
        json!({"kind": "microsandbox", "image": IMAGE})
    );
    preset(micro.preset);

    let process = with("process");
    assert_eq!(process.allow_provider, "local");
    assert_eq!(process.preset["provider"], json!({"kind": "local"}));
}

#[test]
fn plans_refuse_mismatched_runtimes_and_unpinned_or_malformed_values() {
    let cases: [(&str, Value, &str); 9] = [
        (
            "runtime",
            json!("process"),
            "a GKE target runs gvisor or kata",
        ),
        (
            "image",
            json!("ghcr.io/example/harnesses:latest"),
            "@sha256:",
        ),
        ("image", Value::Null, "image is required"),
        ("project", json!("My_Project"), "project must be"),
        ("zone", json!("us-central1"), "zone must name a zone"),
        ("version", json!("latest"), "version must pin"),
        ("master_cidr", json!("172.30.80.0/24"), "/28"),
        ("gke_version", Value::Null, "gke_version is required"),
        ("id", json!("Has Spaces"), "id must be"),
    ];
    for (field, value, expected) in cases {
        let mut body = gke("kata");
        body[field] = value;
        let error = plan(&request(body)).unwrap_err();
        assert!(error.contains(expected), "{field}: {error}");
    }
    let mut body = gke("kata");
    body["platform"] = json!("gce");
    assert!(plan(&request(body))
        .unwrap_err()
        .contains("GCE target runs"));
    assert!(
        serde_json::from_value::<PlanRequest>(json!({"id": "x", "extra": 1})).is_err(),
        "unknown fields are refused"
    );
}

fn app(directory: &tempfile::TempDir) -> Shared {
    let mut config = configured();
    config.data_dir = PathBuf::from(directory.path()).join("data");
    Arc::new(App {
        preview: Arc::new(
            crate::preview::PreviewService::new(directory.path().join("previews")).unwrap(),
        ),
        runtime: Default::default(),
        pull_requests: Default::default(),
        repos: Default::default(),
        registry: crate::ops::Registry::open(
            Box::new(crate::store::MemoryStore::default()),
            crate::ops::Options::new(Vec::new()),
        )
        .unwrap(),
        credentials: crate::auth::Credentials::new(config.all_credentials()),
        shutdown: tokio::sync::watch::channel(false).1,
        storage_idem: Default::default(),
        triggers: Arc::new(crate::triggers::dispatch::Hub::new(
            Arc::new(crate::triggers::store::SqliteTriggers::memory()),
            config.triggers.clone(),
            "http://127.0.0.1".into(),
        )),
        seen: Default::default(),
        companion: None,
        sync: None,
        config,
    })
}

async fn send(
    app: &Shared,
    caller: Caller,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let request = Request::builder().method(method).uri(path);
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    };
    let response = router()
        .layer(Extension(caller))
        .with_state(app.clone())
        .oneshot(request.unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn the_routes_need_admin_and_answer_it() {
    let directory = tempfile::tempdir().unwrap();
    let app = app(&directory);
    let mut reader = Principal::default_for("reader");
    reader.scopes.remove("admin");
    let admin = Principal::default_for("admin");

    let (status, body) = send(
        &app,
        Caller::new(reader.clone()),
        Method::GET,
        "/v1/admin/compute",
        None,
    )
    .await;
    assert_eq!(status, 403, "{body}");
    let (status, _) = send(
        &app,
        Caller::new(reader),
        Method::POST,
        "/v1/admin/compute/plan",
        Some(gke("kata")),
    )
    .await;
    assert_eq!(status, 403);

    let (status, body) = send(
        &app,
        Caller::new(admin.clone()),
        Method::GET,
        "/v1/admin/compute",
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["origin"], "configured");
    assert_eq!(body["targets"].as_array().unwrap().len(), 3);
    assert_eq!(body["targets"][1]["isolation"], "kata");

    let (status, body) = send(
        &app,
        Caller::new(admin.clone()),
        Method::POST,
        "/v1/admin/compute/plan",
        Some(gke("kata")),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["allow_provider"], "agent_sandbox");

    let mut bad = gke("kata");
    bad["runtime"] = json!("process");
    let (status, body) = send(
        &app,
        Caller::new(admin),
        Method::POST,
        "/v1/admin/compute/plan",
        Some(bad),
    )
    .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"]["code"], "invalid_plan");
}

#[tokio::test]
async fn the_page_is_public_and_its_data_is_not() {
    let directory = tempfile::tempdir().unwrap();
    let app = app(&directory);
    let server = crate::api::router(app);
    for (path, kind) in [
        ("/admin", "text/html; charset=utf-8"),
        ("/admin/admin.js", "text/javascript; charset=utf-8"),
    ] {
        let response = server
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(response.headers()["content-type"], kind);
        assert!(response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("script-src 'self'"));
    }
    let response = server
        .oneshot(
            Request::builder()
                .uri("/v1/admin/compute")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert!(!is_public("/admin/other.js"));
}
