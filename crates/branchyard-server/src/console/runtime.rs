//! Native queue commands and operator-owned source resolution.
use super::*;
use crate::api::{App, Caller, RepoState};
use crate::console_store::{Collection, Mutation, Record};
use crate::error::ApiError;
use crate::ops::NewOperation;
use crate::store::Idempotency;
use axum::http::{HeaderMap, StatusCode};
use branchyard_client::api::{Operation, OperationKind, SendRequest, TaskRequest};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// Existing queue work bound to a console workspace and native session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ConsoleWork {
    /// Apply a retained provider action without starting a model turn.
    Runtime {
        /// Stable workspace presentation identity.
        workspace_id: String,
        /// Native session bound at admission.
        session_id: String,
        /// Existing provider action, with native idempotency under the operation ID.
        control: branchyard::workspace_runtime::RuntimeAction,
        /// Exact core host/boot identity required by the admitted allocation.
        owner_host: String,
    },
    /// Publish or refresh PR state through the ordinary fenced worker.
    PullRequest {
        /// Stable workspace presentation identity.
        workspace_id: String,
        /// Native session bound at admission.
        session_id: String,
        /// Candidate-bound service command, separate from its client request identity.
        command: crate::console_pr_gateway::PrCommand,
        /// Exact admitting execution host; no control-server fallback.
        owner_host: String,
    },
    /// Initialize the already reserved native task without starting a turn.
    Initialize {
        /// Stable console workspace identity.
        workspace_id: String,
    },
    /// Start one isolated named native attempt.
    Start {
        /// Stable console workspace identity.
        workspace_id: String,
        /// Stable named native attempt binding.
        session_id: String,
        /// Checked native task request.
        request: Box<TaskRequest>,
    },
    /// Continue the existing attempt and native conversation.
    Send {
        /// Stable console workspace identity.
        workspace_id: String,
        /// Stable named native attempt binding.
        session_id: String,
        /// Checked native continuation request.
        request: Box<SendRequest>,
    },
    /// Deliver cancellation or steering through the native engine.
    Control {
        /// Stable console workspace identity.
        workspace_id: String,
        /// Stable named native attempt binding.
        session_id: String,
        /// Requested native control or lifecycle action.
        control: String,
        /// Steering input attributed to the admitted caller.
        prompt: String,
    },
    /// Reconcile explicit lifecycle intent using retained native state.
    Lifecycle {
        /// Stable console workspace identity.
        workspace_id: String,
        /// Requested native control or lifecycle action.
        intent: String,
    },
}
impl ConsoleWork {
    /// Classify publication by its required merge scope in the existing queue.
    pub(crate) fn operation_kind(&self) -> OperationKind {
        match self {
            Self::PullRequest {
                command: crate::console_pr_gateway::PrCommand::Publish { .. },
                ..
            } => OperationKind::Merge,
            _ => OperationKind::Task,
        }
    }

    pub(super) fn workspace_id(&self) -> &str {
        match self {
            Self::Initialize { workspace_id }
            | Self::Start { workspace_id, .. }
            | Self::Send { workspace_id, .. }
            | Self::Control { workspace_id, .. }
            | Self::Lifecycle { workspace_id, .. } => workspace_id,
            Self::Runtime { workspace_id, .. } | Self::PullRequest { workspace_id, .. } => {
                workspace_id
            }
        }
    }
}
pub(super) fn managed_root(app: &App) -> Result<PathBuf, ApiError> {
    if !app.config.managed_knowledge.enabled {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "knowledge_disabled",
            "managed knowledge storage is disabled",
        ));
    }
    let root = app
        .config
        .managed_knowledge
        .root
        .clone()
        .unwrap_or_else(|| app.config.data_dir.join("knowledge"));
    if !root.is_absolute() {
        return Err(ApiError::internal(
            "managed knowledge root must be absolute",
        ));
    }
    Ok(root)
}
pub(super) fn managed_reservation(
    app: &App,
    workspace: &Record,
) -> Result<branchyard::tasks::TaskReservation, ApiError> {
    let reservation: branchyard::tasks::TaskReservation =
        serde_json::from_value(workspace.body["_allocation"]["reservation"].clone())
            .map_err(|e| ApiError::internal(e.to_string()))?;
    let expected = managed_root(app)?
        .join("tasks")
        .join(text(&workspace.body, "task_id")?)
        .join("work");
    if reservation.work_root() != expected || reservation.id() != text(&workspace.body, "task_id")?
    {
        return Err(ApiError::internal(
            "managed task reservation does not match its workspace binding",
        ));
    }
    Ok(reservation)
}

pub(super) fn resolve_if_initialized(
    app: &App,
    workspace: &Record,
) -> Result<Option<RepoState>, ApiError> {
    if text(&workspace.body, "repo")? == MANAGED_REPO
        && managed_reservation(app, workspace)?
            .inspect()
            .map_err(sdk)?
            .is_none()
    {
        return Ok(None);
    }
    resolve(app, workspace).map(Some)
}

pub(crate) fn resolve(app: &App, workspace: &Record) -> Result<RepoState, ApiError> {
    let name = text(&workspace.body, "repo")?;
    if name != MANAGED_REPO {
        return app.repo(name).cloned();
    }
    let reservation = managed_reservation(app, workspace)?;
    let task = reservation.inspect().map_err(sdk)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "workspace_not_ready",
            "the queued initialization must complete before opening the workspace",
        )
    })?;
    if task.id != reservation.id() || task.files != branchyard::tasks::TaskFiles::NoFiles {
        return Err(ApiError::internal(
            "managed task record does not match its workspace binding",
        ));
    }
    let yard = crate::serve::open_yard(
        &app.config,
        &format!("{MANAGED_REPO}:{}", workspace.id),
        &reservation.work_root(),
    )
    .map_err(ApiError::internal)?;
    crate::serve::configure_yard(&app.config, name, &yard).map_err(ApiError::internal)?;
    // Apply the same operator-owned controls as registered repositories. No request
    // changes executables, connector grants or the operator's configured repo map.
    let feed = crate::feed::Feed::open(yard.clone()).map_err(sdk)?;
    Ok(RepoState {
        name: name.into(),
        yard,
        feed: Arc::new(feed),
        wake: Arc::new(tokio::sync::Notify::new()),
    })
}
pub(super) fn session(
    app: &App,
    caller: &Caller,
    wid: &str,
    sid: &str,
) -> Result<Record, ApiError> {
    let row = get(app, Collection::Sessions, sid)?;
    if row.tenant != caller.tenant() || row.body["workspace_id"] != wid {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such session",
        ));
    }
    Ok(row)
}
pub(super) fn session_subject(session: &Record, caller: &Caller) -> Result<(), ApiError> {
    if session.body["subject"].as_str() != Some(caller.0.connector_subject().as_str()) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "session_subject_required", "native continuation keeps its creator's upstream identity; workspace membership does not delegate another subject's account"));
    }
    Ok(())
}

pub(super) fn cancel_requested(session: &Record) -> bool {
    session.body["_cancel_requested"] == true
}

pub(super) fn session_views(
    app: &App,
    caller: &Caller,
    workspace: &Record,
) -> Result<Vec<Value>, ApiError> {
    let rows = related(app, Collection::Sessions, caller, &workspace.id)?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let repo = resolve(app, workspace)?;
    let task =
        branchyard::tasks::view(&repo.yard, text(&workspace.body, "task_id")?).map_err(sdk)?;
    rows.iter()
        .map(|r| {
            let mut view = public(r);
            match repo.yard.branch(text(&r.body, "branch")?) {
                Ok(branch) => {
                    let mut status = serde_json::to_value(&branch.info().status)
                        .map_err(|e| ApiError::internal(e.to_string()))?;
                    if workspace.body["repo"] == MANAGED_REPO {
                        auth::redact_paths(&mut status, &managed_root(app)?.to_string_lossy());
                    }
                    view["status"] = status["state"].clone();
                    view["status_detail"] = status;
                    view["native_session"] = json!(branch.info().session);
                }
                Err(branchyard::Error::UnknownBranch(_)) => {
                    let operation_id = text(&r.body, "operation_id")?;
                    let operation = app.registry.get(operation_id)?.ok_or_else(|| {
                        ApiError::internal("session initialization operation is missing")
                    })?;
                    view["status"] = serde_json::to_value(operation.state)
                        .map_err(|e| ApiError::internal(e.to_string()))?;
                    if let Some(error) = auth::public_operation(operation).error {
                        view["error"] = serde_json::to_value(error)
                            .map_err(|e| ApiError::internal(e.to_string()))?;
                    }
                }
                Err(e) => return Err(sdk(e)),
            };
            if let Some(attempt) = task
                .attempts
                .iter()
                .find(|attempt| attempt.name == r.body["branch"].as_str().unwrap_or(""))
            {
                view["result_contract"] = serde_json::to_value(&attempt.result_contract)
                    .map_err(|e| ApiError::internal(e.to_string()))?;
                if let Some(result) = &attempt.result {
                    view["result"] = result.clone();
                }
                view["result_event"] = json!(attempt.result_event);
                view["result_error"] = json!(attempt.result_error);
                if workspace.body["repo"] == MANAGED_REPO {
                    auth::redact_paths(
                        &mut view["result_error"],
                        &managed_root(app)?.to_string_lossy(),
                    );
                }
                view["turns"] = json!(attempt.turns);
                view["candidate"] = json!(attempt.candidate);
            }
            if let Some(preset) = super::presets::stored(r)? {
                for field in ["status_detail", "error", "result_error"] {
                    if let Some(value) = view.get_mut(field) {
                        super::presets::project_value(value, &preset.hidden);
                    }
                }
            }
            Ok(view)
        })
        .collect()
}
/// Serialize the existing operation envelope with durable console ownership.
/// An operation on a preset session's branch, or on a branch under one,
/// shows its error and result through the preset's projection.
pub(crate) fn operation_value(app: &App, operation: &Operation) -> Result<Value, ApiError> {
    let mut value = owned_operation_value(app, operation)?;
    let projection = super::Projection::of_operation(app, operation)?;
    for field in ["error", "result"] {
        if let Some(value) = value.get_mut(field) {
            projection.apply(value);
        }
    }
    Ok(value)
}

fn owned_operation_value(app: &App, operation: &Operation) -> Result<Value, ApiError> {
    let operation = auth::public_operation(operation.clone());
    let mut value =
        serde_json::to_value(&operation).map_err(|error| ApiError::internal(error.to_string()))?;
    if let Some(binding) = app
        .registry
        .console_get(Collection::Bindings, &format!("operation:{}", operation.id))?
    {
        let workspace_id = text(&binding.body, "workspace_id")?;
        value["workspace_id"] = json!(workspace_id);
        if let Some(session_id) = binding.body.get("session_id").and_then(Value::as_str) {
            value["session_id"] = json!(session_id);
        } else {
            for branch in &operation.branches {
                if let Some(session) = app.registry.console_get(Collection::Sessions, branch)? {
                    if session.tenant == binding.tenant
                        && session.body["workspace_id"] == workspace_id
                    {
                        value["session_id"] = json!(session.id);
                        break;
                    }
                }
            }
        }
        return Ok(value);
    }
    for branch in &operation.branches {
        if let Some(binding) = app.registry.console_get(
            Collection::Bindings,
            &format!("{}:{branch}", operation.repo),
        )? {
            let workspace_id = text(&binding.body, "workspace_id")?;
            value["workspace_id"] = json!(workspace_id);
            if let Some(session) = app.registry.console_get(Collection::Sessions, branch)? {
                if session.tenant == binding.tenant && session.body["workspace_id"] == workspace_id
                {
                    value["session_id"] = json!(session.id);
                }
            }
            break;
        }
    }
    Ok(value)
}
pub(super) fn replayed(
    app: &App,
    caller: &Caller,
    idem: Option<&Idempotency>,
) -> Result<Option<Value>, ApiError> {
    let Some(idem) = idem else { return Ok(None) };
    if let Some(op) = app.registry.replay(idem, caller.tenant())? {
        auth::authorize_operation(app, caller, &op, "read")?;
        return operation_value(app, &op).map(Some);
    }
    Ok(None)
}
pub(super) fn admit(
    app: &App,
    caller: &Caller,
    headers: &HeaderMap,
    repo: &str,
    command: ConsoleWork,
    idem: Option<Idempotency>,
    mutations: Vec<Mutation>,
) -> Result<Value, ApiError> {
    let branches = match &command {
        ConsoleWork::Initialize { .. } | ConsoleWork::Lifecycle { .. } => Vec::new(),
        ConsoleWork::Start { session_id, .. }
        | ConsoleWork::Send { session_id, .. }
        | ConsoleWork::Control { session_id, .. } => vec![session_id.clone()],
        ConsoleWork::Runtime {
            workspace_id,
            session_id,
            ..
        }
        | ConsoleWork::PullRequest {
            workspace_id,
            session_id,
            ..
        } => {
            vec![text(
                &session(app, caller, workspace_id, session_id)?.body,
                "branch",
            )?
            .into()]
        }
    };
    let locks = match &command {
        ConsoleWork::Control { .. } => Vec::new(),
        ConsoleWork::Initialize { workspace_id } | ConsoleWork::Lifecycle { workspace_id, .. } => {
            // ':' is forbidden in Git refs. This resource namespace cannot
            // collide with any real native branch name in the same queue.
            vec![format!("workspace:{workspace_id}")]
        }
        _ => branches.clone(),
    };
    let policy = app.tenant_policy(caller);
    let mut mutations = mutations;
    let (creates, quota) = match &command {
        ConsoleWork::Start { session_id, .. } => (
            vec![session_id.clone()],
            super::quotas::session_admission(app, caller, &mut mutations)?,
        ),
        ConsoleWork::Control { .. } => (Vec::new(), super::quotas::control_admission(&policy)),
        _ => (Vec::new(), app.admission_quota(caller, &policy)),
    };
    let requires = super::scheduling::requirements(app, caller, repo, &command)?;
    let new = NewOperation {
        repo: repo.into(),
        kind: command.operation_kind(),
        branches,
        cursor: 0,
        locks,
        idempotency: idem,
        principal: caller.0.clone(),
        creates,
        quota,
        requires,
        priority: crate::api::admitted_priority(&policy, None, 0)?,
        trace: crate::api::incoming_trace(headers),
    };
    let value = crate::work::Work::Console {
        command,
        credential: caller.verified().cloned(),
    }
    .to_value()?;
    let (operation, _) = app.registry.submit_console(new, value, &mutations)?;
    operation_value(app, &operation)
}
pub(super) fn operation_idem(
    caller: &Caller,
    headers: &HeaderMap,
    route: &str,
    body: &Value,
) -> Result<Option<Idempotency>, ApiError> {
    let encoded = serde_json::to_string(body).map_err(|e| ApiError::internal(e.to_string()))?;
    let idem = crate::api::idempotency(headers, caller, route, &encoded)?;
    if idem.is_none() {
        return Err(ApiError::bad_request("Idempotency-Key is required"));
    }
    Ok(idem)
}
pub(super) fn start(
    app: &App,
    caller: &Caller,
    wid: &str,
    headers: &HeaderMap,
    body: Value,
) -> Result<Value, ApiError> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        match start_once(app, caller, wid, headers, body.clone()) {
            // The durable metadata and queue transaction rolled back. Re-read
            // authorization, workspace state and quotas, retaining the caller's
            // idempotency key. No native turn has started for this admission.
            Err(error) if error.body.code == "revision_conflict" && attempts < 8 => continue,
            result => return result,
        }
    }
}

fn start_once(
    app: &App,
    caller: &Caller,
    wid: &str,
    headers: &HeaderMap,
    body: Value,
) -> Result<Value, ApiError> {
    let access = body["access"].as_str().unwrap_or("writer");
    let required = if access == "reviewer" {
        "comment"
    } else {
        "run"
    };
    let workspace = auth::authorize_workspace(app, caller, wid, required)?;
    if workspace.body["state"] != "ready" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "workspace_not_ready",
            "wait for initialization or restore the workspace",
        ));
    }
    let idem = operation_idem(
        caller,
        headers,
        &format!("/workspaces/{wid}/sessions"),
        &body,
    )?;
    if let Some(old) = replayed(app, caller, idem.as_ref())? {
        return Ok(old);
    }
    let preset = super::presets::resolve(app, caller, &body)?;
    if !matches!(access, "writer" | "reviewer") {
        return Err(ApiError::bad_request("access must be writer or reviewer"));
    }
    let name = text(&body, "name")?;
    let prompt = text(&body, "prompt")?;
    let harness = text(&body, "harness")?;
    if access == "reviewer" {
        // Acknowledgment cannot replace the promised read-only enforcement.
        branchyard::admit_harness(harness, false).map_err(sdk)?;
    }
    let sid = format!("s-{}", id()?.to_lowercase());
    let mut provision: branchyard::Provisioning =
        match body.get("provision").filter(|value| !value.is_null()) {
            Some(value) => serde_json::from_value(value.clone())
                .map_err(|error| ApiError::bad_request(error.to_string()))?,
            None => branchyard::Provisioning::default(),
        };
    if let Some(preset) = &preset {
        provision.secrets = preset
            .secrets
            .iter()
            .map(|name| branchyard::SecretSource {
                name: name.clone(),
                from: None,
            })
            .collect();
    }
    let model = match &preset {
        Some(preset) => preset.model.clone().map(Value::String),
        None => body.get("model").filter(|value| !value.is_null()).cloned(),
    };
    if let Some(value) = &model {
        let model = value
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| ApiError::bad_request("model must be nonempty text"))?;
        if provision
            .model
            .as_ref()
            .is_some_and(|configured| configured != model)
        {
            return Err(ApiError::bad_request(
                "model conflicts with provision.model",
            ));
        }
        provision.model = Some(model.to_owned());
    }
    if let Some(value) = body.get("effort").filter(|value| !value.is_null()) {
        let effort = serde_json::from_value(value.clone())
            .map_err(|error| ApiError::bad_request(format!("invalid effort: {error}")))?;
        if provision
            .effort
            .is_some_and(|configured| configured != effort)
        {
            return Err(ApiError::bad_request(
                "effort conflicts with provision.effort",
            ));
        }
        provision.effort = Some(effort);
    }
    let policy = match body.get("policy").filter(|value| !value.is_null()) {
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|error| ApiError::bad_request(error.to_string()))?,
        None => branchyard_client::api::PolicySpec::default(),
    };
    let result = body
        .get("result_contract")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let repo = resolve(app, &workspace)?;
    let candidate = review_candidate(app, caller, &workspace, &repo, &body, access)?;
    let request = TaskRequest {
        unapproved_tools: body
            .get("unapproved_tools")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        provider: match &preset {
            Some(preset) => Some(preset.provider.clone()),
            None => body
                .get("provider")
                .filter(|value| !value.is_null())
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|error| ApiError::bad_request(format!("invalid provider: {error}")))?,
        },
        require_labels: preset
            .as_ref()
            .map(|preset| preset.require_labels.clone())
            .unwrap_or_default(),
        base: candidate.as_ref().map(|(commit, _)| commit.clone()),
        prompt: prompt.into(),
        harness: Some(harness.into()),
        name: Some(sid.clone()),
        isolated: true,
        policy,
        provision: Some(provision),
        result,
        ..Default::default()
    };
    crate::work::task_options(app, &repo, &request)?;
    let selected = super::sources::selection(app, caller, wid, &body)?;
    if access == "reviewer"
        && candidate.is_none()
        && body
            .get("artifacts")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    {
        return Err(ApiError::bad_request(
            "reviewer sessions require a frozen workspace candidate or artifact IDs",
        ));
    }
    let artifact_inputs = super::sources::review_inputs(app, caller, &workspace, &repo, &body)?;
    let row = new(
        Collection::Sessions,
        &sid,
        caller,
        json!({"id":sid,"workspace_id":wid,"name":name,"branch":sid,"harness":harness,"model":model,"preset":preset.as_ref().map(|preset|&preset.id),"_preset":preset,"effort":body.get("effort").filter(|value| !value.is_null()),"access":access,"_policy":request.policy,"unapproved_tools":access != "reviewer" && request.unapproved_tools,"review_candidate":candidate.as_ref().map(|(commit,_)|commit),"review_candidate_branch":candidate.as_ref().map(|(_,branch)|branch),"subject":caller.0.connector_subject(),"credential":caller.name(),"status":"queued","sources":selected,"artifacts":artifact_inputs,"result_contract":body.get("result_contract"),"created_at_ms":branchyard_support::time::now_ms()}),
    );
    let mutations = vec![
        insert(row),
        binding(caller, &repo.name, &sid, wid),
        update(workspace.clone(), workspace.body.clone()),
    ];
    admit(
        app,
        caller,
        headers,
        &repo.name,
        ConsoleWork::Start {
            workspace_id: wid.into(),
            session_id: sid,
            request: Box::new(request),
        },
        idem,
        mutations,
    )
}
fn review_candidate(
    app: &App,
    caller: &Caller,
    workspace: &Record,
    repo: &RepoState,
    body: &Value,
    access: &str,
) -> Result<Option<(String, String)>, ApiError> {
    let Some(value) = body.get("candidate").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    if access != "reviewer" {
        return Err(ApiError::bad_request(
            "candidate is supported only for reviewer sessions",
        ));
    }
    let candidate = value
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ApiError::bad_request("candidate must be an exact workspace candidate commit")
        })?;
    let task =
        branchyard::tasks::view(&repo.yard, text(&workspace.body, "task_id")?).map_err(sdk)?;
    for attempt in task.attempts {
        if attempt.candidate.as_deref() == Some(candidate)
            && super::sources::branch_owned(app, caller, workspace, repo, &attempt.name)?
        {
            return Ok(Some((candidate.to_owned(), attempt.name)));
        }
    }
    Err(ApiError::bad_request(
        "candidate must exactly match a current native candidate owned by this workspace",
    ))
}

pub(super) fn continue_session(
    app: &App,
    caller: &Caller,
    wid: &str,
    sid: &str,
    headers: &HeaderMap,
    body: Value,
) -> Result<Value, ApiError> {
    let action = body["action"].as_str().unwrap_or("send");
    if action == "steer" {
        return control(app, caller, wid, sid, headers, "steer", body);
    }
    if !matches!(action, "send" | "resume") {
        return Err(ApiError::bad_request(
            "action must be send, steer or resume",
        ));
    }
    let row = session(app, caller, wid, sid)?;
    let required = if row.body["access"] == "reviewer" {
        "comment"
    } else {
        "run"
    };
    let workspace = auth::authorize_workspace(app, caller, wid, required)?;
    session_subject(&row, caller)?;
    if workspace.body["state"] != "ready" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "workspace_not_ready",
            "restore the workspace first",
        ));
    }
    let repo = resolve(app, &workspace)?;
    let branch = repo.yard.branch(text(&row.body, "branch")?).map_err(sdk)?;
    let policy = row
        .body
        .get("_policy")
        .filter(|value| !value.is_null())
        .cloned()
        .map(serde_json::from_value::<branchyard_client::api::PolicySpec>)
        .transpose()
        .map_err(|error| ApiError::internal(format!("stored session policy: {error}")))?
        .unwrap_or_default();
    let request = SendRequest {
        prompt: text(&body, "prompt")?.into(),
        policy,
        unapproved_tools: row.body["access"] != "reviewer"
            && row.body["unapproved_tools"].as_bool().unwrap_or(false),
        require_labels: super::presets::stored(&row)?
            .map(|preset| preset.require_labels)
            .unwrap_or_default(),
        ..Default::default()
    };
    crate::work::send_options(app, &repo, &request)?;
    crate::work::send_allowed(app, &branch, text(&row.body, "branch")?, &request)?;
    let idem = operation_idem(
        caller,
        headers,
        &format!("/workspaces/{wid}/sessions/{sid}/messages"),
        &body,
    )?;
    admit(
        app,
        caller,
        headers,
        &repo.name,
        ConsoleWork::Send {
            workspace_id: wid.into(),
            session_id: sid.into(),
            request: Box::new(request),
        },
        idem,
        vec![
            update(row.clone(), {
                let mut resumed = row.body.clone();
                resumed["_cancel_requested"] = json!(false);
                resumed
            }),
            update(workspace.clone(), workspace.body.clone()),
        ],
    )
}
pub(super) fn control(
    app: &App,
    caller: &Caller,
    wid: &str,
    sid: &str,
    headers: &HeaderMap,
    action: &str,
    body: Value,
) -> Result<Value, ApiError> {
    let row = session(app, caller, wid, sid)?;
    let required = if row.body["access"] == "reviewer" {
        "comment"
    } else {
        "run"
    };
    let workspace = auth::authorize_workspace(app, caller, wid, required)?;
    if action == "steer" {
        session_subject(&row, caller)?;
    }
    let prompt = if action == "steer" {
        text(&body, "prompt")?.to_owned()
    } else {
        String::new()
    };
    let idem = operation_idem(
        caller,
        headers,
        &format!("/workspaces/{wid}/sessions/{sid}/{action}"),
        &body,
    )?;
    let mut mutations = if action == "cancel" {
        let mut cancelled = row.body.clone();
        cancelled["_cancel_requested"] = json!(true);
        cancelled["_cancel_by"] = json!(caller.acting());
        cancelled["_cancel_subject"] = json!(caller.0.connector_subject());
        cancelled["_cancel_credential"] = json!(caller.name());
        cancelled["_cancel_at_ms"] = json!(branchyard_support::time::now_ms());
        vec![update(row, cancelled)]
    } else {
        Vec::new()
    };
    mutations.push(update(workspace.clone(), workspace.body.clone()));
    admit(
        app,
        caller,
        headers,
        text(&workspace.body, "repo")?,
        ConsoleWork::Control {
            workspace_id: wid.into(),
            session_id: sid.into(),
            control: action.into(),
            prompt,
        },
        idem,
        mutations,
    )
}
pub(super) fn lifecycle(
    app: &App,
    caller: &Caller,
    wid: &str,
    headers: &HeaderMap,
    action: &str,
    body: Value,
) -> Result<Value, ApiError> {
    let workspace = auth::authorize_workspace(app, caller, wid, "run")?;
    if !matches!(action, "archive" | "restore") {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_capability",
            "the bound provider must implement this lifecycle action",
        )
        .detail(json!({"action":action,"reason":"provider_capability_required"})));
    }
    let idem = operation_idem(
        caller,
        headers,
        &format!("/workspaces/{wid}/actions/{action}"),
        &body,
    )?;
    if let Some(old) = replayed(app, caller, idem.as_ref())? {
        return Ok(old);
    }
    revision(&workspace, &body)?;
    let repo = resolve_if_initialized(app, &workspace)?;
    if repo.is_none() && action != "restore" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "workspace_not_ready",
            "restore the incomplete workspace before archiving",
        ));
    }
    ensure_idle(app, caller, &workspace, repo.as_ref(), None)?;
    let repo_name = text(&workspace.body, "repo")?.to_owned();
    let command = if action == "restore" {
        ConsoleWork::Initialize {
            workspace_id: wid.into(),
        }
    } else {
        ConsoleWork::Lifecycle {
            workspace_id: wid.into(),
            intent: action.into(),
        }
    };
    let mut intent = workspace.body.clone();
    intent["state"] = json!(if action == "archive" {
        "archiving"
    } else {
        "restoring"
    });
    admit(
        app,
        caller,
        headers,
        &repo_name,
        command,
        idem,
        vec![update(workspace, intent)],
    )
}
pub(super) fn ensure_idle(
    app: &App,
    caller: &Caller,
    workspace: &Record,
    repo: Option<&RepoState>,
    ignore_operation: Option<&str>,
) -> Result<(), ApiError> {
    if app
        .runtime
        .get()
        .is_some_and(|runtime| runtime.has_manual_writer(&workspace.id, None))
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "workspace_busy",
            "close the workspace's terminal before changing its lifecycle",
        ));
    }
    let sessions = related(app, Collection::Sessions, caller, &workspace.id)?;
    let mut resources = std::collections::BTreeSet::new();
    for session in &sessions {
        resources.insert(text(&session.body, "branch")?.to_owned());
    }
    for stored in app.registry.unfinished(caller.tenant())? {
        let operation = stored.operation;
        let owned = app
            .registry
            .console_get(Collection::Bindings, &format!("operation:{}", operation.id))?
            .is_some_and(|binding| {
                binding.tenant == workspace.tenant && binding.body["workspace_id"] == workspace.id
            });
        if Some(operation.id.as_str()) != ignore_operation
            && operation.repo == text(&workspace.body, "repo")?
            && (owned
                || operation
                    .branches
                    .iter()
                    .any(|branch| resources.contains(branch)))
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "workspace_busy",
                "settle queued and running workspace operations before changing lifecycle",
            ));
        }
    }
    let Some(repo) = repo else {
        if sessions.is_empty() {
            return Ok(());
        }
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "workspace_not_ready",
            "workspace session bindings require an initialized native task",
        ));
    };
    for session in sessions {
        let branch = match repo.yard.branch(text(&session.body, "branch")?) {
            Ok(branch) => branch,
            Err(branchyard::Error::UnknownBranch(_)) => continue,
            Err(error) => return Err(sdk(error)),
        };
        if matches!(
            branch.info().status,
            branchyard::BranchStatus::Running
                | branchyard::BranchStatus::Waiting
                | branchyard::BranchStatus::WaitingOnChildren
        ) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "workspace_busy",
                "cancel or settle active and waiting turns before changing workspace lifecycle",
            ));
        }
    }
    Ok(())
}

/// Derive execution state from native attempts and the durable allocation operation.
pub(super) fn workspace_view(
    app: &App,
    workspace: &Record,
    sessions: &[Value],
) -> Result<Value, ApiError> {
    let mut view = public(workspace);
    view["lifecycle_intent"] = workspace.body["state"].clone();
    if workspace.body["state"] == "ready"
        && sessions.iter().any(|session| {
            matches!(
                session["status"].as_str(),
                Some("running" | "waiting_on_children")
            )
        })
    {
        view["state"] = json!("running");
    } else if matches!(
        workspace.body["state"].as_str(),
        Some("pending" | "restoring" | "archiving")
    ) {
        if let Some(id) = workspace.body["_operation_id"].as_str() {
            let operation = app.registry.get(id)?.ok_or_else(|| {
                ApiError::internal("workspace initialization operation is missing")
            })?;
            view["operation_id"] = json!(id);
            if matches!(
                operation.state,
                branchyard_client::api::OperationState::Failed
                    | branchyard_client::api::OperationState::Interrupted
            ) {
                view["state"] = serde_json::to_value(operation.state)
                    .map_err(|e| ApiError::internal(e.to_string()))?;
                view["error"] = json!(auth::public_operation(operation).error);
            }
        }
    }
    Ok(view)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuation_subject_is_stable_across_paired_devices() {
        let principal = crate::config::Principal::default_for("creator");
        let session = Record {
            collection: Collection::Sessions,
            id: "session".into(),
            tenant: principal.tenant.clone(),
            revision: 1,
            body: json!({"subject":principal.connector_subject()}),
        };
        assert!(session_subject(&session, &Caller::new(principal.clone())).is_ok());
        assert!(session_subject(
            &session,
            &Caller::new(principal.delegated_to("second-device".into()))
        )
        .is_ok());
        assert!(session_subject(
            &session,
            &Caller::new(crate::config::Principal::default_for("another-editor"))
        )
        .is_err());
    }
}
