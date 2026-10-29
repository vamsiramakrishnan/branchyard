//! Live console capabilities for the existing runtime and PR gateways.

use std::sync::{Arc, Weak};

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use branchyard::workspace_runtime::RuntimeAction;
use branchyard_client::api::{Operation, OperationResult};
use serde_json::{json, Value};

use super::{auth, get, runtime, sdk, text, update, Collection, ConsoleWork};
use crate::api::{App, Caller, RepoState};
use crate::console_pr::{Action, AuthorizedBranch, Error, Service};
use crate::console_pr_gateway::{PrAccess, PrCommand, PrGateway};
use crate::console_runtime::{
    AuthorizedWorkspace, RuntimeAccess, RuntimeGateway, RuntimeGatewayOptions,
};
use crate::console_store::{Mutation, Record};
use crate::error::ApiError;

/// Server-resolved source and the exact metadata revisions used for admission.
pub(super) struct Target {
    workspace: Record,
    session: Record,
    member: Record,
    binding: Record,
    repo: RepoState,
}

/// Upgrade only for one authorization/admission call; gateways never own the app.
fn upgrade(weak: &Weak<App>) -> Result<Arc<App>, ApiError> {
    weak.upgrade().ok_or_else(ApiError::shutting_down)
}

/// Intersect current credential, scope, membership, source and native binding.
pub(super) fn target(
    app: &App,
    caller: &Caller,
    wid: &str,
    sid: &str,
    scope: &str,
    creator: bool,
) -> Result<Target, ApiError> {
    app.check_caller(caller)?;
    // Capture before authorization, then CAS this exact grant during admission.
    let subject = caller.0.connector_subject();
    let member = get(app, Collection::Members, &format!("{wid}:{subject}"))?;
    let workspace = auth::authorize_workspace(app, caller, wid, scope)?;
    let session = runtime::session(app, caller, wid, sid)?;
    if member.tenant != caller.tenant()
        || member.body["workspace_id"] != wid
        || member.body["subject"] != subject
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "workspace_access_denied",
            "workspace grant does not match its subject",
        ));
    }
    if scope != "read" {
        if workspace.body["state"] != "ready" {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "workspace_not_ready",
                "restore the workspace before changing its runtime or PR",
            ));
        }
        if session.body["access"] != "writer"
            || !matches!(member.body["role"].as_str(), Some("owner" | "editor"))
        {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "writer_required",
                "manual runtime and PR actions require a writer session and workspace editor grant",
            ));
        }
        if creator {
            runtime::session_subject(&session, caller)?;
        }
    }
    let repo = runtime::resolve(app, &workspace)?;
    let branch = text(&session.body, "branch")?;
    if scope != "read" {
        let provider = repo
            .yard
            .branch(branch)
            .map_err(sdk)?
            .provider()
            .map_err(sdk)?;
        let _checked_provider = app.provider(provider)?;
    }
    let binding = get(
        app,
        Collection::Bindings,
        &format!("{}:{branch}", repo.name),
    )?;
    if binding.tenant != workspace.tenant
        || binding.body["workspace_id"] != workspace.id
        || binding.body["repo"] != repo.name
        || binding.body["branch"] != branch
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "workspace_binding_changed",
            "the native session ownership binding is invalid",
        ));
    }
    Ok(Target {
        workspace,
        session,
        member,
        binding,
        repo,
    })
}

/// Preserve the exact native host/boot representation used by registry workers.
fn owner(app: &App, target: &Target) -> Result<String, ApiError> {
    let state = target
        .repo
        .yard
        .workspace_runtime_state(text(&target.session.body, "branch")?)
        .map_err(sdk)?;
    match state.runtime.owner_host {
        Some(host) if host == app.registry.worker().host => Ok(host),
        _ => Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "runtime_owner_unavailable",
            "connect to the worker that owns this workspace; no control-server fallback is available",
        )),
    }
}

/// Resolve runtime access while allowing inspection of a foreign owner's reason.
fn runtime_target(
    app: &App,
    caller: &Caller,
    wid: &str,
    sid: &str,
    access: RuntimeAccess,
) -> Result<AuthorizedWorkspace, ApiError> {
    let inspect = access == RuntimeAccess::Inspect;
    let target = target(
        app,
        caller,
        wid,
        sid,
        if inspect { "read" } else { "run" },
        access == RuntimeAccess::Terminal,
    )?;
    if !inspect {
        owner(app, &target)?;
    }
    Ok(AuthorizedWorkspace {
        yard: target.repo.yard,
        branch: text(&target.session.body, "branch")?.into(),
    })
}

/// Select the original required scope for every live PR service recheck.
fn pr_scope(access: PrAccess) -> &'static str {
    match access {
        PrAccess::Inspect => "read",
        PrAccess::Refresh => "run",
        PrAccess::Publish => "merge",
    }
}

/// Resolve a service context whose repeated authorization retains no strong App.
fn pr_target(
    weak: &Weak<App>,
    caller: &Caller,
    wid: &str,
    sid: &str,
    access: PrAccess,
) -> Result<AuthorizedBranch, ApiError> {
    let current = upgrade(weak)?;
    let resolved = target(
        &current,
        caller,
        wid,
        sid,
        pr_scope(access),
        access != PrAccess::Inspect,
    )?;
    if access != PrAccess::Inspect {
        owner(&current, &resolved)?;
    }
    let branch = text(&resolved.session.body, "branch")?.to_owned();
    let root = resolved.repo.yard.root().to_path_buf();
    let weak = weak.clone();
    let caller = caller.clone();
    let wid = wid.to_owned();
    let sid = sid.to_owned();
    let expected = branch.clone();
    AuthorizedBranch::checked(
        resolved.repo.yard,
        &branch,
        if access == PrAccess::Publish {
            Action::Publish
        } else {
            Action::Read
        },
        move |_| {
            let current = upgrade(&weak).map_err(pr_error)?;
            let target = target(
                &current,
                &caller,
                &wid,
                &sid,
                pr_scope(access),
                access != PrAccess::Inspect,
            )
            .map_err(pr_error)?;
            if access != PrAccess::Inspect {
                owner(&current, &target).map_err(pr_error)?;
            }
            if target.repo.yard.root() != root || target.session.body["branch"] != expected {
                return Err(Error::denied("the native session binding changed"));
            }
            Ok(())
        },
    )
    .map_err(|error| ApiError::new(StatusCode::FORBIDDEN, &error.code, error.message))
}

/// Preserve safe authorization categories inside the independently checked PR service.
fn pr_error(error: ApiError) -> Error {
    Error::new(error.body.code.clone(), error.body.message.clone(), false)
}

/// Admit under the existing transaction and branch locks, with metadata CAS guards.
pub(super) fn queue(
    app: &App,
    caller: &Caller,
    target: Target,
    command: ConsoleWork,
    identity: Value,
    key: &str,
) -> Result<Operation, ApiError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "idempotency-key",
        HeaderValue::from_str(key).map_err(|_| ApiError::bad_request("invalid Idempotency-Key"))?,
    );
    let route = format!(
        "/workspaces/{}/sessions/{}/provider",
        target.workspace.id, target.session.id
    );
    let idem = runtime::operation_idem(caller, &headers, &route, &identity)?;
    let value = if let Some(existing) = runtime::replayed(app, caller, idem.as_ref())? {
        existing
    } else {
        if let ConsoleWork::Runtime { control, .. } = &command {
            let state = target
                .repo
                .yard
                .workspace_runtime_state(text(&target.session.body, "branch")?)
                .map_err(sdk)?;
            let supported = match control {
                RuntimeAction::Pause => state.can_pause,
                RuntimeAction::Suspend => state.can_suspend,
                RuntimeAction::Resume => state.can_resume,
                RuntimeAction::Destroy => state.can_destroy,
            };
            if !supported {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "unsupported",
                    "the provider does not support this action on its current retained allocation",
                ));
            }
        }
        let guards = vec![
            target.workspace,
            target.member,
            target.session,
            target.binding,
        ]
        .into_iter()
        .map(|record| {
            let body = record.body.clone();
            update(record, body)
        })
        .collect::<Vec<Mutation>>();
        runtime::admit(
            app,
            caller,
            &headers,
            &target.repo.name,
            command,
            idem,
            guards,
        )?
    };
    serde_json::from_value(value).map_err(|error| ApiError::internal(error.to_string()))
}

/// Initialize gateways exactly once after App construction, before dispatcher startup.
pub(crate) fn install(app: &Arc<App>) -> Result<(), ApiError> {
    let resolve = Arc::downgrade(app);
    let admit = resolve.clone();
    let runtime = RuntimeGateway::new(
        RuntimeGatewayOptions {
            allow_client_commands: app.config.allow_client_commands,
            ..RuntimeGatewayOptions::default()
        },
        Arc::new(move |caller, wid, sid, access| {
            runtime_target(upgrade(&resolve)?.as_ref(), caller, wid, sid, access)
        }),
        Some(Arc::new(move |caller, wid, sid, control, key| {
            let app = upgrade(&admit)?;
            let target = target(&app, caller, wid, sid, "run", false)?;
            let owner_host = owner(&app, &target)?;
            let command = ConsoleWork::Runtime {
                workspace_id: wid.into(),
                session_id: sid.into(),
                control,
                owner_host,
            };
            queue(
                &app,
                caller,
                target,
                command,
                json!({"runtime":control}),
                key,
            )
        })),
    )?;
    app.runtime
        .set(Arc::new(runtime))
        .map_err(|_| ApiError::internal("workspace runtime was already initialized"))?;
    let resolve = Arc::downgrade(app);
    let admit = resolve.clone();
    let service = Service::new(app.config.by_path.clone().unwrap_or_else(|| "by".into()));
    let pr = PrGateway::new(
        service,
        Arc::new(move |caller, wid, sid, access| pr_target(&resolve, caller, wid, sid, access)),
        Some(Arc::new(move |caller, wid, sid, command, key| {
            let app = upgrade(&admit)?;
            let scope = if matches!(command, PrCommand::Publish { .. }) {
                "merge"
            } else {
                "run"
            };
            let target = target(&app, caller, wid, sid, scope, true)?;
            let owner_host = owner(&app, &target)?;
            let identity = command.request_identity();
            let command = ConsoleWork::PullRequest {
                workspace_id: wid.into(),
                session_id: sid.into(),
                command,
                owner_host,
            };
            queue(&app, caller, target, command, identity, key)
        })),
    );
    app.pull_requests
        .set(Arc::new(pr))
        .map_err(|_| ApiError::internal("workspace PR service was already initialized"))?;
    Ok(())
}

/// Recheck the captured host before any claimed worker can touch the allocation.
fn execution_owner(app: &App, target: &Target, expected: &str) -> Result<(), ApiError> {
    if owner(app, target)? != expected {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "runtime_owner_unavailable",
            "the execution owner changed after admission",
        ));
    }
    Ok(())
}

/// Run provider controls synchronously in the existing claimed operation worker.
pub(super) fn execute_runtime(
    app: &App,
    caller: &Caller,
    wid: &str,
    sid: &str,
    control: RuntimeAction,
    expected: &str,
    operation_id: &str,
) -> Result<OperationResult, ApiError> {
    let target = target(app, caller, wid, sid, "run", false)?;
    execution_owner(app, &target, expected)?;
    let gateway = app
        .runtime
        .get()
        .ok_or_else(|| ApiError::internal("workspace runtime is not initialized"))?;
    let state = gateway.execute_lifecycle(caller, wid, sid, control, operation_id)?;
    Ok(OperationResult {
        runtime: Some(
            serde_json::to_value(state).map_err(|error| ApiError::internal(error.to_string()))?,
        ),
        ..OperationResult::default()
    })
}

/// Drive async forge I/O on this worker's runtime, without blocking the server driver.
pub(super) fn execute_pr(
    app: &App,
    caller: &Caller,
    wid: &str,
    sid: &str,
    command: &PrCommand,
    expected: &str,
    operation_id: &str,
) -> Result<OperationResult, ApiError> {
    let scope = if matches!(command, PrCommand::Publish { .. }) {
        "merge"
    } else {
        "run"
    };
    let target = target(app, caller, wid, sid, scope, true)?;
    execution_owner(app, &target, expected)?;
    let gateway = app
        .pull_requests
        .get()
        .ok_or_else(|| ApiError::internal("workspace PR service is not initialized"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| ApiError::internal("PR worker runtime could not start"))?;
    let result = runtime
        .block_on(gateway.execute(caller, wid, sid, command, operation_id))
        .map_err(|failure| {
            let code = if failure.error.unknown_effect {
                "pr_effect_uncertain"
            } else {
                failure.error.code.as_str()
            };
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                code,
                failure.error.message.clone(),
            )
            .detail(json!({
                "operation_id": failure.operation_id,
                "unknown_effect": failure.error.unknown_effect,
                "cause": failure.error.code,
            }))
        })?;
    Ok(OperationResult {
        pull_request: Some(result.result),
        ..OperationResult::default()
    })
}
