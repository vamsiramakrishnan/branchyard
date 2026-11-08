//! The admin compute console (`docs/admin-console.md`): one place where an
//! operator sees the compute this server can place turns on, and plans new
//! compute, while developers keep connecting with `by --remote`.
//!
//! - `GET /admin` and `GET /admin/admin.js`, public: the console's page. It
//!   holds no data; it asks for an `admin` token and calls the routes below.
//! - `GET /v1/admin/compute` (`admin`): every execution preset read as a
//!   compute target (platform, isolation, network, image), whether its
//!   provider kind is allowed, and the live workers whose labels can claim
//!   its turns.
//! - `POST /v1/admin/compute/plan` (`admin`): for a GCE worker or a GKE
//!   Agent Sandbox pool with a chosen runtime, the packaged setup helper's
//!   plan and apply commands, the worker label, and the execution preset to
//!   add. It runs nothing: the commands are run by the operator, with their
//!   own cloud credentials, so this server never holds project IAM.
//!
//! Neither route opens a provider, contacts a cloud API, runs a helper or
//! calls a model.

use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::{blocking, Caller, JsonBody, Shared};
use crate::config::{Config, ExecutionPreset};
use crate::error::ApiError;
use crate::store::LiveWorker;

const PAGE: &str = include_str!("admin_compute/index.html");
const SCRIPT: &str = include_str!("admin_compute/admin.js");

/// The page loads only its own script and talks only to this origin.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; \
    connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route("/v1/admin/compute", get(report))
        .route("/v1/admin/compute/plan", post(plan_route))
}

/// The console's page, outside `/v1`, needing no token.
pub(crate) fn public() -> Router<Shared> {
    Router::new()
        .route(
            "/admin",
            get(|| async { asset(PAGE, "text/html; charset=utf-8") }),
        )
        .route(
            "/admin/admin.js",
            get(|| async { asset(SCRIPT, "text/javascript; charset=utf-8") }),
        )
}

/// Whether `path` is one of the console page's own files.
pub(crate) fn is_public(path: &str) -> bool {
    matches!(path, "/admin" | "/admin/admin.js")
}

fn asset(body: &'static str, kind: &'static str) -> Response {
    let mut response = body.into_response();
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE.as_str(), kind),
        ("content-security-policy", CSP),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("x-frame-options", "DENY"),
        ("cache-control", "no-store"),
    ] {
        headers.insert(
            header::HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    response
}

fn need_admin(caller: &Caller) -> Result<(), ApiError> {
    match caller.0.allows("admin") {
        true => Ok(()),
        false => Err(crate::api::scope_required("admin")),
    }
}

/// Where a target's turns run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Platform {
    /// On the worker that claims the turn: this server or a `by worker`.
    Worker,
    /// In a Kubernetes Agent Sandbox the claiming worker creates on GKE.
    Gke,
    /// In a Substrate actor on its cluster.
    Substrate,
}

/// What separates a turn from the worker's kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Isolation {
    /// A process as the worker's user: none.
    Process,
    /// gVisor's user-space kernel (Docker `runsc`, or GKE Sandbox).
    Gvisor,
    /// Linux namespaces sharing the worker's kernel.
    Runc,
    /// A Microsandbox KVM microVM on the worker.
    Microsandbox,
    /// GKE's managed Kata Containers with Cloud Hypervisor.
    Kata,
    /// Whatever the Substrate template's actor runtime is.
    Substrate,
    /// A recipe's own environment.
    Recipe,
    /// A provider this console does not describe yet.
    Unknown,
}

/// One execution preset, read as compute.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Target {
    pub preset: String,
    pub label: String,
    /// The provider kind, as `allow_providers` names it.
    pub provider: &'static str,
    pub platform: Platform,
    pub isolation: Isolation,
    /// `deny` or `unrestricted` for native providers; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// The image the allocation boots, for providers that take one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The Kubernetes namespace of an Agent Sandbox target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub harnesses: Vec<String>,
    pub require_labels: Vec<String>,
    /// Absent: every tenant.
    pub tenants: Option<Vec<String>>,
    /// Whether the provider kind is in `allow_providers`.
    pub allowed: bool,
    /// Live workers whose labels include every required label.
    pub workers: Vec<String>,
    /// `ready`, `provider_not_allowed` or `no_worker`.
    pub state: &'static str,
    /// What the operator does next; fixed text.
    pub next: &'static str,
}

/// A live worker and the targets it can claim.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Worker {
    pub id: String,
    pub host: String,
    pub labels: Vec<String>,
    pub seen_ms_ago: u64,
    /// Whether it is this server's own dispatcher.
    pub this: bool,
    pub targets: Vec<String>,
}

/// `GET /v1/admin/compute`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Report {
    /// `configured` when the operator wrote `execution_presets`, else
    /// `default`: the implicit local preset.
    pub origin: &'static str,
    pub targets: Vec<Target>,
    pub workers: Vec<Worker>,
}

struct Shape {
    platform: Platform,
    isolation: Isolation,
    network: Option<String>,
    image: Option<String>,
    namespace: Option<String>,
}

fn plain(platform: Platform, isolation: Isolation) -> Shape {
    Shape {
        platform,
        isolation,
        network: None,
        image: None,
        namespace: None,
    }
}

/// Read from the provider's name and its serde form, the wire contract,
/// so a new provider variant needs no change here
/// (`crates/branchyard/tests/provider_seam.rs`).
fn shape(provider: &branchyard::Provider) -> Shape {
    let value = serde_json::to_value(provider).unwrap_or_default();
    let field = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let runtime = field("runtime");
    match provider.name() {
        "local" => plain(Platform::Worker, Isolation::Process),
        "recipe" => plain(Platform::Worker, Isolation::Recipe),
        "substrate" => plain(Platform::Substrate, Isolation::Substrate),
        "microsandbox" => Shape {
            image: provider.image(),
            ..plain(Platform::Worker, Isolation::Microsandbox)
        },
        "container" => Shape {
            network: field("network"),
            image: provider.image(),
            ..plain(
                Platform::Worker,
                match runtime.as_deref() {
                    Some("runc") => Isolation::Runc,
                    _ => Isolation::Gvisor,
                },
            )
        },
        "agent_sandbox" => Shape {
            network: field("network"),
            image: provider.image(),
            namespace: field("namespace"),
            ..plain(
                Platform::Gke,
                match runtime.as_deref() {
                    Some("microvm") => Isolation::Kata,
                    _ => Isolation::Gvisor,
                },
            )
        },
        _ => plain(Platform::Worker, Isolation::Unknown),
    }
}

/// What a target is read from: a configured preset, or the implicit local one.
struct Source<'a> {
    id: &'a str,
    label: &'a str,
    provider: &'static str,
    shape: Shape,
    harnesses: Vec<String>,
    require_labels: &'a [String],
    tenants: Option<Vec<String>>,
}

impl<'a> Source<'a> {
    fn of(preset: &'a ExecutionPreset) -> Source<'a> {
        Source {
            id: &preset.id,
            label: &preset.label,
            provider: preset.provider.name(),
            shape: shape(&preset.provider),
            harnesses: preset.harnesses.iter().map(|h| h.harness.clone()).collect(),
            require_labels: &preset.require_labels,
            tenants: preset.tenants.clone(),
        }
    }
}

/// Whether a worker carrying `labels` may claim a turn requiring `required`,
/// as the queue's claim decides it.
fn claims(labels: &[String], required: &[String]) -> bool {
    required.iter().all(|label| labels.contains(label))
}

/// Read the configured presets (or the implicit local one) as compute.
pub(crate) fn compute(config: &Config, workers: &[LiveWorker], this: &str) -> Report {
    let (origin, sources): (&'static str, Vec<Source>) = match config.execution_presets.is_empty() {
        false => (
            "configured",
            config.execution_presets.iter().map(Source::of).collect(),
        ),
        true => (
            "default",
            vec![Source {
                id: "local",
                label: "This server",
                provider: "local",
                shape: plain(Platform::Worker, Isolation::Process),
                harnesses: Vec::new(),
                require_labels: &[],
                tenants: None,
            }],
        ),
    };
    let targets: Vec<Target> = sources
        .into_iter()
        .map(|source| {
            let provider = source.provider;
            let allowed = provider == "local" || config.allow_providers.contains(provider);
            let claimers: Vec<String> = workers
                .iter()
                .filter(|w| claims(&w.labels, source.require_labels))
                .map(|w| w.id.clone())
                .collect();
            let (state, next) = match (allowed, claimers.is_empty()) {
                (false, _) => (
                    "provider_not_allowed",
                    "add the provider kind to allow_providers and restart the server",
                ),
                (true, true) => (
                    "no_worker",
                    "start a worker carrying every required label: by worker --label LABEL",
                ),
                (true, false) => (
                    "ready",
                    "start a session on this preset; the runtime is checked when a turn starts",
                ),
            };
            let shape = source.shape;
            Target {
                preset: source.id.to_owned(),
                label: source.label.to_owned(),
                provider,
                platform: shape.platform,
                isolation: shape.isolation,
                network: shape.network,
                image: shape.image,
                namespace: shape.namespace,
                harnesses: source.harnesses,
                require_labels: source.require_labels.to_vec(),
                tenants: source.tenants,
                allowed,
                workers: claimers,
                state,
                next,
            }
        })
        .collect();
    let mut workers: Vec<Worker> = workers
        .iter()
        .map(|w| Worker {
            id: w.id.clone(),
            host: w.host.clone(),
            labels: w.labels.clone(),
            seen_ms_ago: w.seen_ms_ago,
            this: w.id == this,
            targets: targets
                .iter()
                .filter(|t| t.workers.contains(&w.id))
                .map(|t| t.preset.clone())
                .collect(),
        })
        .collect();
    workers.sort_by_key(|w| (!w.this, w.id.clone()));
    Report {
        origin,
        targets,
        workers,
    }
}

async fn report(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Report>, ApiError> {
    need_admin(&caller)?;
    let shared = app.clone();
    let workers = blocking(move || shared.registry.live_workers())
        .await?
        .map_err(|e| ApiError::internal(format!("could not read the live workers: {e}")))?;
    let this = app.registry.worker().id.clone();
    Ok(Json(compute(&app.config, &workers, &this)))
}

/// The compute to plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanPlatform {
    /// A dedicated Compute Engine VM (`by compute gce`).
    Gce,
    /// A dedicated private GKE cluster with Agent Sandbox pools
    /// (`tools/agent_sandbox_gke.py`).
    Gke,
}

/// The runtime a planned target isolates turns with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanRuntime {
    /// GCE only: harness processes as the worker's user.
    Process,
    /// gVisor: Docker `runsc` on GCE, GKE Sandbox on GKE.
    Gvisor,
    /// GCE only: Microsandbox KVM microVMs.
    Microsandbox,
    /// GKE only: managed Kata Containers with Cloud Hypervisor.
    Kata,
}

/// `POST /v1/admin/compute/plan`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanRequest {
    /// The preset ID to create, and the worker label `compute:ID`.
    pub id: String,
    pub platform: PlanPlatform,
    pub runtime: PlanRuntime,
    pub project: String,
    pub zone: String,
    /// The VM (GCE) or cluster (GKE) name.
    pub name: String,
    /// The Branchyard release to install.
    pub version: String,
    /// The allocation image, pinned `@sha256:`; GKE and GCE gVisor only.
    #[serde(default)]
    pub image: Option<String>,
    /// GKE: a complete available GKE version.
    #[serde(default)]
    pub gke_version: Option<String>,
    /// GKE: an existing node service account.
    #[serde(default)]
    pub service_account: Option<String>,
    /// GKE: a private IPv4 `/28` for the control plane.
    #[serde(default)]
    pub master_cidr: Option<String>,
    /// GCE: reach SSH through IAP.
    #[serde(default)]
    pub tunnel_through_iap: bool,
    /// Harnesses the preset offers; default `claude-code`.
    #[serde(default)]
    pub harnesses: Vec<String>,
}

/// One command of a plan, as argv: the console never builds a shell line.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Step {
    pub title: &'static str,
    /// Where it runs: `operator` (a machine with cloud credentials) or
    /// `worker` (the machine that dispatches the target's turns).
    pub on: &'static str,
    pub argv: Vec<String>,
}

/// The answer to a plan request.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Plan {
    pub steps: Vec<Step>,
    /// The label the dispatching worker carries and the preset requires.
    pub label: String,
    /// The provider kind to add to `allow_providers`.
    pub allow_provider: &'static str,
    /// The entry to add to `execution_presets`.
    pub preset: Value,
    /// Fixed text the operator reads before applying.
    pub notes: Vec<&'static str>,
}

fn resource_name(field: &str, value: &str) -> Result<(), String> {
    let ok = (1..=63).contains(&value.len())
        && value.starts_with(|c: char| c.is_ascii_lowercase())
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    ok.then_some(())
        .ok_or_else(|| format!("{field} must be a lowercase Google Cloud resource name"))
}

fn release(value: &str) -> Result<(), String> {
    let parts: Vec<&str> = value.split('.').collect();
    let ok = parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    ok.then_some(())
        .ok_or_else(|| "version must pin a Branchyard release, such as 0.5.0".to_owned())
}

fn digest_image(value: Option<&str>) -> Result<String, String> {
    let image = value.ok_or("image is required for this runtime")?;
    let digest = image.rsplit_once("@sha256:").map_or("", |(_, d)| d);
    let ok = !image.starts_with('@')
        && digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        && !image.chars().any(char::is_whitespace);
    ok.then(|| image.to_owned())
        .ok_or_else(|| "image must be pinned by @sha256: digest".to_owned())
}

fn required<'a>(field: &str, value: Option<&'a str>) -> Result<&'a str, String> {
    match value {
        Some(v) if !v.is_empty() && !v.chars().any(|c| c.is_whitespace() || c.is_control()) => {
            Ok(v)
        }
        _ => Err(format!("{field} is required for a GKE target")),
    }
}

fn preset_id(value: &str) -> Result<(), String> {
    let ok = (1..=55).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    ok.then_some(()).ok_or_else(|| {
        "id must be 1 to 55 of a-z, 0-9 and '-' (it becomes the label compute:ID)".to_owned()
    })
}

/// The plan for `request`, or why it cannot be planned.
pub(crate) fn plan(request: &PlanRequest) -> Result<Plan, String> {
    preset_id(&request.id)?;
    resource_name("project", &request.project)?;
    resource_name("name", &request.name)?;
    let zone_ok = request.zone.matches('-').count() == 2;
    resource_name("zone", &request.zone)?;
    if !zone_ok {
        return Err("zone must name a zone such as us-central1-a".into());
    }
    release(&request.version)?;
    let harnesses = match request.harnesses.is_empty() {
        true => vec!["claude-code".to_owned()],
        false => request.harnesses.clone(),
    };
    if let Some(bad) = harnesses
        .iter()
        .find(|h| h.is_empty() || !h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    {
        return Err(format!("harness {bad:?} is not a harness ID"));
    }
    let label = format!("compute:{}", request.id);
    let flag = |name: &str, value: &str| format!("--{name}={value}");
    let preset_harnesses: Vec<Value> = harnesses.iter().map(|h| json!({"harness": h})).collect();
    match (request.platform, request.runtime) {
        (PlanPlatform::Gke, runtime @ (PlanRuntime::Gvisor | PlanRuntime::Kata)) => {
            let image = digest_image(request.image.as_deref())?;
            let gke_version = required("gke_version", request.gke_version.as_deref())?;
            let account = required("service_account", request.service_account.as_deref())?;
            let cidr = required("master_cidr", request.master_cidr.as_deref())?;
            if !cidr.ends_with("/28") {
                return Err("master_cidr must be a private IPv4 /28".into());
            }
            let base = vec![
                "python3".to_owned(),
                "tools/agent_sandbox_gke.py".to_owned(),
                flag("project", &request.project),
                flag("zone", &request.zone),
                flag("cluster", &request.name),
                flag("version", gke_version),
                flag("service-account", account),
                flag("master-cidr", cidr),
            ];
            let mut apply = base.clone();
            apply.push("--apply".into());
            let (namespace, runtime) = match runtime {
                PlanRuntime::Kata => ("branchyard-microvm", "microvm"),
                _ => ("branchyard-gvisor", "gvisor"),
            };
            let context = format!("gke_{}_{}_{}", request.project, request.zone, request.name);
            Ok(Plan {
                steps: vec![
                    Step { title: "Review the offline plan", on: "operator", argv: base },
                    Step { title: "Create the cluster, pools, controller and policies", on: "operator", argv: apply },
                    Step {
                        title: "Run the dispatching server with the cluster's kubeconfig and the label",
                        on: "worker",
                        argv: vec!["by".into(), "serve".into(), "--label".into(), label.clone()],
                        // KUBECONFIG names the copied kubeconfig on that host.
                    },
                ],
                allow_provider: "agent_sandbox",
                preset: json!({
                    "id": request.id,
                    "label": format!("GKE {} ({})", request.name, if runtime == "microvm" { "Kata" } else { "gVisor" }),
                    "provider": {"kind": "agent_sandbox", "context": context, "namespace": namespace,
                        "image": image, "runtime": runtime, "network": "deny"},
                    "harnesses": preset_harnesses,
                    "require_labels": [label],
                }),
                label,
                notes: vec![
                    "The cluster's API endpoint is private: apply from a host with a route to it.",
                    "Copy the kubeconfig the helper writes in its state directory to the dispatching worker.",
                    "Networking stays deny until you choose otherwise; model access needs an explicit choice.",
                    "Resources stay billable until you delete them. See docs/agent-sandbox-gke.md.",
                ],
            })
        }
        (
            PlanPlatform::Gce,
            runtime @ (PlanRuntime::Process | PlanRuntime::Gvisor | PlanRuntime::Microsandbox),
        ) => {
            let mut base = vec![
                "by".to_owned(),
                "compute".to_owned(),
                "gce".to_owned(),
                flag("project", &request.project),
                flag("zone", &request.zone),
                flag("name", &request.name),
                flag("version", &request.version),
            ];
            match runtime {
                PlanRuntime::Gvisor => base.push("--gvisor".into()),
                PlanRuntime::Microsandbox => {
                    base.push("--backend=microsandbox".into());
                    base.push("--machine-type=n2-standard-4".into());
                }
                _ => {}
            }
            if request.tunnel_through_iap {
                base.push("--tunnel-through-iap".into());
            }
            let mut apply = base.clone();
            apply.push("--apply".into());
            let provider = match runtime {
                PlanRuntime::Gvisor => {
                    json!({"kind": "container", "image": digest_image(request.image.as_deref())?,
                    "runtime": "gvisor", "network": "deny"})
                }
                PlanRuntime::Microsandbox => {
                    json!({"kind": "microsandbox", "image": digest_image(request.image.as_deref())?})
                }
                _ => json!({"kind": "local"}),
            };
            let allow_provider = match runtime {
                PlanRuntime::Gvisor => "container",
                PlanRuntime::Microsandbox => "microsandbox",
                _ => "local",
            };
            Ok(Plan {
                steps: vec![
                    Step { title: "Review the offline plan", on: "operator", argv: base },
                    Step { title: "Create and bootstrap the VM", on: "operator", argv: apply },
                    Step {
                        title: "Run a worker on the VM with the label",
                        on: "worker",
                        argv: ["by", "worker", "--database", "DATABASE_URL", "--repo", "REPO=PATH", "--label"]
                            .into_iter()
                            .map(str::to_owned)
                            .chain([label.clone()])
                            .collect(),
                    },
                ],
                allow_provider,
                preset: json!({
                    "id": request.id,
                    "label": format!("GCE {}", request.name),
                    "provider": provider,
                    "harnesses": preset_harnesses,
                    "require_labels": [label],
                }),
                label,
                notes: vec![
                    "A worker shares this server's --database and serves the same checkouts; see docs/server.md#several-servers-on-one-database.",
                    "Without a shared database, use the VM as its own server: by --remote with the URL deployment.json records.",
                    "Resources stay billable until you delete them. See docs/compute-gce.md.",
                ],
            })
        }
        (PlanPlatform::Gke, _) => Err("a GKE target runs gvisor or kata".into()),
        (PlanPlatform::Gce, _) => Err("a GCE target runs process, gvisor or microsandbox".into()),
    }
}

async fn plan_route(
    Extension(caller): Extension<Caller>,
    JsonBody(request, _): JsonBody<PlanRequest>,
) -> Result<Json<Plan>, ApiError> {
    need_admin(&caller)?;
    plan(&request)
        .map(Json)
        .map_err(|message| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_plan", message))
}

#[cfg(test)]
mod tests;
