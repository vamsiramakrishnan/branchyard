//! Claimed console operations executed through the existing native engine.

use super::runtime::{
    cancel_requested, ensure_idle, managed_reservation, resolve, resolve_if_initialized, session,
    session_subject, ConsoleWork,
};
use super::*;
use crate::ops::Finished;
use crate::store::StoredOperation;
use branchyard_client::api::OperationResult;

/// Execute through the existing fenced registry; no console command replays a turn.
pub(crate) fn execute(
    app: &App,
    stored: &StoredOperation,
    command: ConsoleWork,
    caller: Caller,
) -> Finished {
    let run = || -> Result<OperationResult, ApiError> {
        app.check_caller(&caller)?;
        let required = if matches!(
            &command,
            ConsoleWork::PullRequest {
                command: crate::console_pr_gateway::PrCommand::Publish { .. },
                ..
            }
        ) {
            "merge"
        } else if let ConsoleWork::Start { session_id, .. }
        | ConsoleWork::Send { session_id, .. }
        | ConsoleWork::Control { session_id, .. } = &command
        {
            let row = session(app, &caller, command.workspace_id(), session_id)?;
            if row.body["access"] == "reviewer" {
                "comment"
            } else {
                "run"
            }
        } else {
            "run"
        };
        let workspace = auth::authorize_workspace(app, &caller, command.workspace_id(), required)?;
        if matches!(
            &command,
            ConsoleWork::Initialize { .. } | ConsoleWork::Lifecycle { .. }
        ) {
            allocation_owner(&workspace, &stored.operation.id)?;
        }
        if matches!(&command, ConsoleWork::Initialize { .. }) {
            let existing = resolve_if_initialized(app, &workspace)?;
            ensure_idle(
                app,
                &caller,
                &workspace,
                existing.as_ref(),
                Some(&stored.operation.id),
            )?;
            if text(&workspace.body, "repo")? == MANAGED_REPO {
                let reservation = managed_reservation(app, &workspace)?;
                branchyard::tasks::initialize_reserved(&reservation).map_err(sdk)?;
            }
        }
        let repo = resolve(app, &workspace)?;
        if matches!(
            &command,
            ConsoleWork::Start { .. } | ConsoleWork::Send { .. }
        ) && workspace.body["state"] != "ready"
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "workspace_not_ready",
                "restore the workspace before starting or continuing a session",
            ));
        }
        match command {
            ConsoleWork::Runtime {
                workspace_id,
                session_id,
                control,
                owner_host,
            } => super::services::execute_runtime(
                app,
                &caller,
                &workspace_id,
                &session_id,
                control,
                &owner_host,
                &stored.operation.id,
            ),
            ConsoleWork::PullRequest {
                workspace_id,
                session_id,
                command,
                owner_host,
            } => super::services::execute_pr(
                app,
                &caller,
                &workspace_id,
                &session_id,
                &command,
                &owner_host,
                &stored.operation.id,
            ),
            ConsoleWork::Initialize { .. } => {
                ensure_idle(
                    app,
                    &caller,
                    &workspace,
                    Some(&repo),
                    Some(&stored.operation.id),
                )?;
                if workspace.body["_allocation"]["kind"] == "repository" {
                    let task =
                        serde_json::from_value(workspace.body["_allocation"]["task"].clone())
                            .map_err(|e| ApiError::internal(e.to_string()))?;
                    branchyard::tasks::initialize_repository(&repo.yard, &task).map_err(sdk)?;
                }
                complete_workspace_state(app, &workspace, &stored.operation.id, "ready")?;
                Ok(OperationResult::default())
            }
            ConsoleWork::Start {
                session_id,
                request,
                ..
            } => run_native_turn(app, &repo, std::slice::from_ref(&session_id), || {
                let session = session(app, &caller, &workspace.id, &session_id)?;
                session_subject(&session, &caller)?;
                if cancel_requested(&session) {
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        "session_cancelled",
                        "this queued session was cancelled before its native turn started",
                    ));
                }
                if let Some(candidate) = session.body["review_candidate"].as_str() {
                    if session.body["access"] != "reviewer"
                        || request.base.as_deref() != Some(candidate)
                        || !super::sources::branch_owned(
                            app,
                            &caller,
                            &workspace,
                            &repo,
                            text(&session.body, "review_candidate_branch")?,
                        )?
                    {
                        return Err(ApiError::new(StatusCode::FORBIDDEN, "review_candidate_binding_invalid", "the frozen review candidate no longer has a valid workspace ownership binding"));
                    }
                } else if request.base.is_some() {
                    return Err(ApiError::bad_request(
                        "a console base requires a frozen reviewer candidate binding",
                    ));
                }
                let mut options = crate::work::task_options(app, &repo, &request)?;
                options.join_task = Some(text(&workspace.body, "task_id")?.into());
                options.actor = Some(branchyard::connectors::Actor {
                    subject: caller.0.connector_subject(),
                    tenant: caller.tenant().into(),
                });
                super::sources::prepare(app, &caller, &workspace, &session, &repo, &mut options)?;
                cancel_observer(app, &repo, &session_id, &mut options);
                if session.body["access"] == "reviewer" {
                    let harness = options.harness.as_deref().ok_or_else(|| {
                        ApiError::bad_request("reviewer work requires an enforcing named harness")
                    })?;
                    branchyard::admit_harness(harness, false).map_err(sdk)?;
                    options.unapproved_tools = false;
                    options.policy = reviewer_policy(options.policy);
                }
                // The engine appends the requested result to a first turn itself.
                let prompt = super::harness_input::Additions::first(&workspace.id, &session, None)
                    .compose(&request.prompt);
                repo.yard.task(prompt).options(options).run().map_err(sdk)?;
                Ok(OperationResult::default())
            }),
            ConsoleWork::Send {
                session_id,
                request,
                ..
            } => run_native_turn(app, &repo, std::slice::from_ref(&session_id), || {
                let session = session(app, &caller, &workspace.id, &session_id)?;
                session_subject(&session, &caller)?;
                if cancel_requested(&session) {
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        "session_cancelled",
                        "this queued session was cancelled before its native turn started",
                    ));
                }
                let mut options = crate::work::send_options(app, &repo, &request)?;
                let branch = repo
                    .yard
                    .branch(text(&session.body, "branch")?)
                    .map_err(sdk)?;
                if session.body["access"] == "reviewer" {
                    let harness = options.harness.as_deref().unwrap_or(&branch.info().harness);
                    branchyard::admit_harness(harness, false).map_err(sdk)?;
                    options.unapproved_tools = false;
                    options.policy = reviewer_policy(options.policy);
                }
                crate::work::send_allowed(app, &branch, text(&session.body, "branch")?, &request)?;
                cancel_observer(app, &repo, &session_id, &mut options);
                let prompt = super::harness_input::Additions::later(
                    branch.result_contract().map_err(sdk)?.as_ref(),
                )
                .compose(&request.prompt);
                branch.send(&prompt, options).map_err(sdk)?;
                Ok(OperationResult::default())
            }),
            ConsoleWork::Control {
                session_id,
                control: action,
                prompt,
                ..
            } => {
                let row = session(app, &caller, &workspace.id, &session_id)?;
                let branch = text(&row.body, "branch")?;
                if action == "cancel" {
                    if !cancel_requested(&row)
                        || row.body["_cancel_operation_id"].as_str()
                            != Some(stored.operation.id.as_str())
                    {
                        // An explicit continuation or newer cancellation superseded
                        // this already durable request. Never cancel the newer turn.
                        return Ok(OperationResult::default());
                    }
                    match repo.yard.cancel_as(branch, &caller.acting()) {
                        Ok(_) => {}
                        Err(branchyard::Error::UnknownBranch(_)) if cancel_requested(&row) => {}
                        Err(error) => return Err(sdk(error)),
                    }
                } else {
                    session_subject(&row, &caller)?;
                    repo.yard
                        .steer_as(branch, &prompt, &caller.acting())
                        .map_err(sdk)?;
                }
                Ok(OperationResult::default())
            }
            ConsoleWork::Lifecycle { intent: action, .. } => {
                ensure_idle(
                    app,
                    &caller,
                    &workspace,
                    Some(&repo),
                    Some(&stored.operation.id),
                )?;
                if action == "restore" && workspace.body["_allocation"]["kind"] == "repository" {
                    let task =
                        serde_json::from_value(workspace.body["_allocation"]["task"].clone())
                            .map_err(|e| ApiError::internal(e.to_string()))?;
                    branchyard::tasks::initialize_repository(&repo.yard, &task).map_err(sdk)?;
                }
                complete_workspace_state(
                    app,
                    &workspace,
                    &stored.operation.id,
                    if action == "archive" {
                        "archived"
                    } else {
                        "ready"
                    },
                )?;
                Ok(OperationResult::default())
            }
        }
    };
    Finished {
        result: run().map_err(|e| *e.body),
        end_cursor: None,
    }
}

fn cancel_observer(
    app: &App,
    repo: &crate::api::RepoState,
    session_id: &str,
    options: &mut branchyard::TaskOptions,
) {
    let original = options.observer.take();
    let registry = app.registry.clone();
    let yard = repo.yard.clone();
    let session_id = session_id.to_owned();
    options.observer = Some(std::sync::Arc::new(move |event| {
        if matches!(
            event.activity,
            branchyard::Activity::Status(branchyard::BranchStatus::Running)
                | branchyard::Activity::Prompt(_)
        ) {
            let reason = match registry.console_get(Collection::Sessions, &session_id) {
                Ok(Some(row)) if cancel_requested(&row) => Some(
                    row.body["_cancel_by"]
                        .as_str()
                        .unwrap_or("the workspace cancellation intent")
                        .to_owned(),
                ),
                Ok(Some(_)) => None,
                Ok(None) => Some("the session binding is missing".into()),
                Err(error) => Some(format!(
                    "the session cancellation intent could not be checked: {}",
                    error.body.message
                )),
            };
            if let Some(reason) = reason {
                branchyard_support::best_effort(
                    "apply durable workspace session cancellation",
                    yard.cancel_as(&event.branch, &reason),
                );
            }
        }
        if let Some(observer) = &original {
            observer(event);
        }
    }));
}

/// Preserve native sync ownership around each model-bearing console turn.
fn run_native_turn(
    app: &App,
    repo: &crate::api::RepoState,
    branches: &[String],
    run: impl FnOnce() -> Result<OperationResult, ApiError>,
) -> Result<OperationResult, ApiError> {
    let Some(sync) = &app.sync else {
        return run();
    };
    if repo.name == MANAGED_REPO || !app.repos.contains_key(&repo.name) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "sync_source_unsupported",
            "the configured sync service does not serve this managed source",
        )
        .detail(json!({"reason":"managed_source_sync_mapping_required"})));
    }
    let leases = sync.begin(&repo.name, branches).map_err(|refusal| {
        let (status, code, message) = match refusal {
            crate::sync::Refusal::Held(message) => {
                (StatusCode::CONFLICT, "sync_lease_held", message)
            }
            crate::sync::Refusal::Unavailable(message) => {
                (StatusCode::SERVICE_UNAVAILABLE, "sync_unavailable", message)
            }
        };
        ApiError::new(status, code, message)
    })?;
    let (result, lost) = crate::work::run_under_leases(app, repo, branches, &leases, run);
    sync.end(&repo.name, branches);
    drop(leases);
    match lost {
        Some(reason) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "sync_lease_lost",
            format!("{reason}; the run was cancelled and its result is not accepted"),
        )),
        None => result,
    }
}

/// Finish only this operation's lifecycle intent while preserving concurrent metadata edits.
pub(super) fn complete_workspace_state(
    app: &App,
    original: &Record,
    operation_id: &str,
    state: &str,
) -> Result<(), ApiError> {
    // Only the idempotent metadata completion is retried. Filesystem allocation
    // and every native model turn remain outside this loop.
    for _ in 0..8 {
        let current = get(app, Collection::Workspaces, &original.id)?;
        if current.tenant != original.tenant {
            return Err(ApiError::internal("workspace completion tenant mismatch"));
        }
        allocation_owner(&current, operation_id)?;
        let mut body = current.body.clone();
        body["state"] = json!(state);
        match app.registry.console_mutate(&[update(current, body)]) {
            Err(error) if error.body.code == "revision_conflict" => continue,
            result => return result,
        }
    }
    Err(ApiError::new(
        StatusCode::CONFLICT,
        "revision_conflict",
        "workspace metadata remained busy; restore the reserved workspace to finish initialization",
    ))
}

fn allocation_owner(workspace: &Record, operation_id: &str) -> Result<(), ApiError> {
    if workspace.body["_operation_id"].as_str() != Some(operation_id) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "stale_workspace_operation",
            "a newer workspace lifecycle operation owns this allocation",
        ));
    }
    Ok(())
}

/// Intersect the admitted caller policy with read-only review constraints.
fn reviewer_policy(original: branchyard::Policy) -> branchyard::Policy {
    let read_only = branchyard::read_only_policy();
    branchyard::Policy::ask(
        move |branch, request| match read_only.decide(branch, request) {
            branchyard::PermissionDecision::Allow => original.decide(branch, request),
            denied => denied,
        },
    )
    .with_preset(Some(branchyard::PolicyPreset::ReadOnly))
}

#[cfg(test)]
mod tests {
    use super::{allocation_owner, reviewer_policy};
    use branchyard::{PermissionDecision, PermissionKey, PermissionRequest, Policy};
    use serde_json::json;

    #[test]
    fn only_the_admitted_lifecycle_operation_owns_allocation_effects() {
        let mut workspace = crate::console_store::Record {
            collection: crate::console_store::Collection::Workspaces,
            id: "workspace".into(),
            tenant: "tenant".into(),
            revision: 1,
            body: json!({"_operation_id":"current"}),
        };
        assert!(allocation_owner(&workspace, "current").is_ok());
        assert!(allocation_owner(&workspace, "previous").is_err());
        workspace.body = json!({});
        assert!(allocation_owner(&workspace, "current").is_err());
    }

    #[test]
    fn reviewer_policy_preserves_caller_denials_and_forbids_mutation() {
        let request = |tool: &str| PermissionRequest {
            key: PermissionKey("review".into()),
            tool: tool.into(),
            input: json!({}),
        };
        let denied = reviewer_policy(Policy::deny_all());
        assert!(matches!(
            denied.decide("reviewer", &request("Read")),
            PermissionDecision::Deny { .. }
        ));
        let review = reviewer_policy(Policy::allow_all().deny("Read"));
        for tool in ["Read", "Write", "Bash", "unknown_external_tool"] {
            assert!(matches!(
                review.decide("reviewer", &request(tool)),
                PermissionDecision::Deny { .. }
            ));
        }
        assert_eq!(
            review.decide("reviewer", &request("Grep")),
            PermissionDecision::Allow
        );
    }
}
