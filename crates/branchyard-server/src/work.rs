//! What an accepted operation runs, as data: the queue row's description,
//! which any worker on the operation store can execute.
//!
//! A description holds the request as the client sent it, plus what the
//! accepting server resolved and locked for it (planned names, a merge
//! target, a delegating parent). It holds no secret values: a request
//! names secrets, and the executing server reads them from its own
//! configuration, as it resolves commands, providers and policies. Servers
//! sharing a store are expected to share that configuration; one that
//! would refuse the request fails the operation with the same error the
//! request would have got from it.
//!
//! `Work::run` is the executor: it rebuilds the engine's options with
//! the same rules the handlers check at admission, and calls the SDK.

use branchyard_support::LockExt as _;
use std::sync::Arc;

use axum::http::StatusCode;
use branchyard::{Branch, Spawn, TaskOptions, Yard};
use branchyard_client::api::{
    ErrorBody, ForkRequest, MapRequest, OperationKind, OperationResult, ReincarnateRequest,
    SendRequest, SpawnRequest, TaskRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{App, RepoState};
use crate::console::Projection;
use crate::error::{self, ApiError};
use crate::ops::{Executor, Finished};
use crate::store::StoredOperation;

/// Every kind of operation, as its queue row describes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Work {
    /// Durable workspace command using native task/attempt execution.
    Console {
        /// Native action bound to its admitted workspace and session.
        command: crate::console::ConsoleWork,
        /// Captured credential reference; no bearer token is stored.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential: Option<crate::companion::Verified>,
    },
    /// `POST /v1/repos/{repo}/tasks`, or a trigger's run, which `trigger`
    /// names on the task it starts.
    Task {
        request: TaskRequest,
        /// The trigger run that admitted it, recorded on the task it starts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trigger: Option<branchyard::tasks::TaskTrigger>,
    },
    /// `POST .../branches/{branch}/send`.
    Send {
        branch: String,
        request: SendRequest,
    },
    /// `POST .../branches/{branch}/fork`.
    Fork {
        branch: String,
        request: ForkRequest,
    },
    /// `POST .../branches/{branch}/reincarnate`.
    Reincarnate {
        branch: String,
        request: ReincarnateRequest,
    },
    /// A trigger's run to a branch its lifecycle made dormant: the branch's
    /// harness session starts over on the same worktree for `reason`
    /// (`Branch::reincarnate_in_place`), then the send runs as a fresh
    /// session. Listed as a reincarnation.
    Resume {
        /// The dormant branch.
        branch: String,
        /// The run's send, as `Work::Send` has it.
        request: SendRequest,
        /// Why the session starts over, for the branch's log.
        reason: String,
    },
    /// `POST .../branches/{branch}/merge`, with the target resolved at
    /// admission.
    Merge { branch: String, target: String },
    /// `POST .../branches/{parent}/spawn`, with the child's name fixed at
    /// admission when a name or seat decides it.
    Spawn {
        parent: String,
        name: Option<String>,
        request: SpawnRequest,
    },
    /// `POST .../branches/{branch}/integrate`, into the parent found at
    /// admission, with the siblings integrated together with it.
    Integrate {
        branch: String,
        parent: String,
        /// Siblings integrated together with `branch`, after it.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        with: Vec<String>,
    },
    /// `POST .../branches/{branch}/plan/approve`.
    ApprovePlan {
        branch: String,
        request: branchyard_client::knowledge_api::PlanApproveRequest,
    },
    /// `POST .../branches/{branch}/plan/reject`.
    RejectPlan {
        branch: String,
        request: branchyard_client::knowledge_api::PlanRejectRequest,
    },
    /// `POST /v1/repos/{repo}/maps` or `.../maps/{name}/resume`, with the
    /// items a resume takes from the recorded map.
    Map { request: MapRequest },
    /// `POST .../branches/{branch}/check`: the branch's check on its
    /// current work, as `by check` runs it. Takes no branch lock and runs
    /// no turn.
    Check {
        /// The branch whose check runs.
        branch: String,
    },
}

impl Work {
    pub fn kind(&self) -> OperationKind {
        match self {
            Work::Console { command, .. } => command.operation_kind(),
            Work::Task { .. } => OperationKind::Task,
            Work::Send { .. } => OperationKind::Send,
            Work::Fork { .. } => OperationKind::Fork,
            Work::Reincarnate { .. } | Work::Resume { .. } => OperationKind::Reincarnate,
            Work::Merge { .. } => OperationKind::Merge,
            Work::Spawn { .. } => OperationKind::Spawn,
            Work::Integrate { .. } => OperationKind::Integrate,
            Work::ApprovePlan { .. } => OperationKind::ApprovePlan,
            Work::RejectPlan { .. } => OperationKind::RejectPlan,
            Work::Map { .. } => OperationKind::Map,
            Work::Check { .. } => OperationKind::Check,
        }
    }

    /// The queue row's JSON.
    pub fn to_value(&self) -> Result<Value, ApiError> {
        serde_json::to_value(self)
            .map_err(|e| ApiError::internal(format!("could not describe the operation: {e}")))
    }

    /// Run the work against `repo` on `app`'s configuration.
    fn run(
        self,
        app: &App,
        repo: &RepoState,
        principal: Option<&crate::config::Principal>,
        trace: Option<&str>,
        admitted_ms: u64,
        projection: &mut Projection,
    ) -> Result<OperationResult, ErrorBody> {
        let trace_parent = trace.map(str::to_owned);
        let sdk = |e: branchyard::Error| *error::sdk(&e).body;
        let api = |e: ApiError| *e.body;
        let yard = &repo.yard;
        // A new branch acts for the principal that asked for it at the
        // connector gateway; see docs/connectors.md.
        let actor = principal.map(|p| branchyard::connectors::Actor {
            subject: p.connector_subject(),
            tenant: p.tenant.clone(),
        });
        match self {
            Work::Console { .. } => Err(*ApiError::internal(
                "console work must resolve its workspace before execution",
            )
            .body),
            Work::Task { request, trigger } => {
                // A preset is checked again here, for the principal that
                // admitted the task: the operator may have removed it.
                let tenant = principal.map(|p| p.tenant.as_str());
                let (request, options, preset_projection) =
                    preset_or_task_options(app, repo, &request, tenant).map_err(api)?;
                projection.extend(preset_projection);
                let options = TaskOptions {
                    actor: actor.clone(),
                    trigger,
                    trace_parent: trace_parent.clone(),
                    admitted_ms: Some(admitted_ms),
                    ..options
                };
                let builder = yard.task(request.prompt.clone()).options(options);
                let ran = match request.harnesses.is_empty() {
                    true => builder.run().map(|b| vec![b]),
                    false => {
                        let ids: Vec<&str> = request.harnesses.iter().map(String::as_str).collect();
                        builder.run_on(&ids)
                    }
                };
                ran.and_then(finished)
                    .map_err(|error| projection.error(sdk(error)))
            }
            Work::Send { branch, request } => {
                let options = TaskOptions {
                    trace_parent: trace_parent.clone(),
                    admitted_ms: Some(admitted_ms),
                    ..send_options(app, repo, &request).map_err(api)?
                };
                let target = yard.branch(&branch).map_err(sdk)?;
                send_allowed(app, &target, &branch, &request).map_err(api)?;
                let prompt = sent_prompt(&target, &request).map_err(api)?;
                record_feedback(&target, &prompt, &request).map_err(sdk)?;
                target
                    .send(&prompt, options)
                    .and_then(|b| finished(vec![b]))
                    .map_err(sdk)
            }
            Work::Fork { branch, request } => {
                let source = yard.branch(&branch).map_err(sdk)?;
                app.provider(source.provider().map_err(sdk)?).map_err(api)?;
                let options = TaskOptions {
                    actor: actor.clone(),
                    trace_parent: trace_parent.clone(),
                    ..fork_options(app, repo, &request).map_err(api)?
                };
                yard.branch(&branch)
                    .and_then(|source| match request.at {
                        Some(turn) => source.fork_at(turn, &request.prompt, options),
                        None => source.fork(&request.prompt, request.fresh_session, options),
                    })
                    .and_then(|b| finished(vec![b]))
                    .map_err(sdk)
            }
            Work::Reincarnate { branch, request } => {
                let source = yard.branch(&branch).map_err(sdk)?;
                app.provider(source.provider().map_err(sdk)?).map_err(api)?;
                let options = TaskOptions {
                    actor: actor.clone(),
                    trace_parent: trace_parent.clone(),
                    ..reincarnate_options(app, repo, &request).map_err(api)?
                };
                yard.branch(&branch)
                    .and_then(|source| source.reincarnate(options))
                    .and_then(|b| finished(vec![b]))
                    .map_err(sdk)
            }
            Work::Resume {
                branch,
                request,
                reason,
            } => {
                let options = TaskOptions {
                    trace_parent: trace_parent.clone(),
                    ..send_options(app, repo, &request).map_err(api)?
                };
                let target = yard.branch(&branch).map_err(sdk)?;
                send_allowed(app, &target, &branch, &request).map_err(api)?;
                let prompt = sent_prompt(&target, &request).map_err(api)?;
                target.reincarnate_in_place(&reason).map_err(sdk)?;
                record_feedback(&target, &prompt, &request).map_err(sdk)?;
                target
                    .send(&prompt, options)
                    .and_then(|b| finished(vec![b]))
                    .map_err(sdk)
            }
            Work::Merge { branch, target } => {
                let merged = yard.merge(&branch, &target).map_err(sdk)?;
                let branches = yard
                    .branch(&branch)
                    .map(|b| vec![b.info().clone()])
                    .unwrap_or_default();
                Ok(OperationResult {
                    branches,
                    merged: Some(merged),
                    ..OperationResult::default()
                })
            }
            Work::Spawn {
                parent,
                name,
                request,
            } => {
                let (mut options, spawn) = spawn_parts(app, repo, &request, name).map_err(api)?;
                let source = yard.branch(&parent).map_err(sdk)?;
                app.provider(source.provider().map_err(sdk)?).map_err(api)?;
                options.trace_parent = trace_parent.clone();
                let run = || {
                    let source = yard.branch(&parent)?;
                    let delegate = source.delegate(options)?;
                    let spawned = delegate.spawn(spawn)?;
                    source.wait_subtree()?;
                    let inspection = delegate.inspect(&spawned.name)?;
                    let info = yard.branch(&spawned.name)?.info().clone();
                    Ok::<_, branchyard::Error>(OperationResult {
                        branches: vec![info],
                        inspection: Some(inspection),
                        ..OperationResult::default()
                    })
                };
                run().map_err(sdk)
            }
            Work::ApprovePlan { branch, request } => {
                let options = TaskOptions {
                    trace_parent: trace_parent.clone(),
                    ..plan_send_options(app, repo, &request.send).map_err(api)?
                };
                let by = person(principal);
                yard.approve_plan(&branch, request.edited.as_deref(), &by, &options)
                    .and_then(|b| finished(vec![b]))
                    .map_err(sdk)
            }
            Work::RejectPlan { branch, request } => {
                let options = TaskOptions {
                    trace_parent: trace_parent.clone(),
                    ..plan_send_options(app, repo, &request.send).map_err(api)?
                };
                let by = person(principal);
                yard.reject_plan(
                    &branch,
                    request.reason.as_deref(),
                    request.replan,
                    &by,
                    &options,
                )
                .and_then(|b| finished(vec![b]))
                .map_err(sdk)
            }
            Work::Map { request } => {
                let spec = map_spec(&request).map_err(api)?;
                let options = branchyard::MapOptions {
                    task: TaskOptions {
                        actor: actor.clone(),
                        trace_parent: trace_parent.clone(),
                        ..task_options(app, repo, &request.task).map_err(api)?
                    },
                    retry_failed: request.retry_failed,
                    ..branchyard::MapOptions::default()
                };
                let report = yard.map(spec, &options).map_err(sdk)?;
                // The branches that answered, or were tried last, where
                // they remain.
                let branches = report
                    .rows
                    .iter()
                    .filter_map(|row| row.branch.as_deref())
                    .chain(report.reduce.as_ref().and_then(|r| r.branch.as_deref()))
                    .filter_map(|name| yard.branch(name).ok())
                    .map(|b| b.info().clone())
                    .collect();
                Ok(OperationResult {
                    branches,
                    map: Some(report),
                    ..OperationResult::default()
                })
            }
            Work::Check { branch } => {
                let run = || {
                    let options = TaskOptions {
                        observer: Some(crate::api::observer(&repo.wake)),
                        ..TaskOptions::default()
                    };
                    let target = yard.branch(&branch)?;
                    let report = target.delegate(options)?.check(&branch)?;
                    Ok::<_, branchyard::Error>(OperationResult {
                        branches: vec![target.info().clone()],
                        check_report: Some(report),
                        ..OperationResult::default()
                    })
                };
                run().map_err(sdk)
            }
            Work::Integrate {
                branch,
                parent,
                with,
            } => {
                let run = || {
                    let options = TaskOptions {
                        observer: Some(crate::api::observer(&repo.wake)),
                        ..TaskOptions::default()
                    };
                    let delegate = yard.branch(&parent)?.delegate(options)?;
                    if with.is_empty() {
                        let merged = delegate.integrate(&branch)?;
                        return Ok::<_, branchyard::Error>(OperationResult {
                            branches: vec![yard.branch(&branch)?.info().clone()],
                            merged: Some(merged),
                            ..OperationResult::default()
                        });
                    }
                    let mut names = vec![branch.as_str()];
                    names.extend(with.iter().map(String::as_str));
                    let merged = delegate.integrate_all(&names)?;
                    let branches = names
                        .iter()
                        .map(|name| Ok(yard.branch(name)?.info().clone()))
                        .collect::<Result<Vec<_>, branchyard::Error>>()?;
                    Ok(OperationResult {
                        branches,
                        merged_all: Some(merged),
                        ..OperationResult::default()
                    })
                };
                run().map_err(sdk)
            }
        }
    }
}

/// Runs claimed operations on a server's repositories and configuration.
pub struct AppExecutor(pub Arc<App>);

/// Who a turn stopped for a shutdown was cancelled by, in its log.
pub const SHUTDOWN: &str = "the server's shutdown";

impl AppExecutor {
    fn shutdown_repo(
        &self,
        stored: &StoredOperation,
        branch: &str,
    ) -> Result<Option<RepoState>, ApiError> {
        let app = &self.0;
        let name = &stored.operation.repo;
        if let Some(repo) = app.repos.get(name) {
            return Ok(Some(repo.clone()));
        }
        if name != crate::console::MANAGED_REPO {
            return Ok(None);
        }
        use crate::console_store::Collection;
        let Some(binding) = app
            .registry
            .console_get(Collection::Bindings, &format!("{name}:{branch}"))?
        else {
            return Ok(None);
        };
        let workspace_id = binding.body["workspace_id"]
            .as_str()
            .ok_or_else(|| ApiError::internal("shutdown binding has no workspace"))?;
        let Some(workspace) = app
            .registry
            .console_get(Collection::Workspaces, workspace_id)?
        else {
            return Ok(None);
        };
        if binding.tenant != stored.tenant()
            || workspace.tenant != stored.tenant()
            || binding.body["repo"] != name.as_str()
            || binding.body["branch"] != branch
            || workspace.body["repo"] != name.as_str()
        {
            return Err(ApiError::internal(
                "shutdown workspace binding differs from its operation",
            ));
        }
        crate::console::resolve_source(app, &workspace).map(Some)
    }
}

impl Executor for AppExecutor {
    fn projection(&self, stored: &StoredOperation, work: &Value) -> Result<Projection, ApiError> {
        let Some(principal) = &stored.principal else {
            return Ok(Projection::default());
        };
        let parsed = serde_json::from_value::<Work>(work.clone());
        if matches!(&parsed, Ok(Work::Console { .. })) {
            // Console sessions retain their own durable preset binding and
            // cursor lifecycle; this hook extends direct repository work.
            return Ok(Projection::default());
        }
        let Ok(Work::Task { request, .. }) = parsed else {
            return Projection::of_operation(&self.0, &stored.operation);
        };
        if request.preset.is_none() {
            return Projection::of_operation(&self.0, &stored.operation);
        }
        // This only hides configuration. Execution still rechecks every
        // permission and refuses unresolved presets before any effect.
        // Capture before the running record so concurrent branch/feed reads
        // see the worker's values even when they changed since admission.
        match crate::console::for_task(&self.0, &principal.tenant, &request) {
            Ok((_, preset)) => Ok(Projection::of_preset(&preset)),
            Err(_) => Ok(Projection::default()),
        }
    }

    fn stop_for_shutdown(&self, stored: &StoredOperation) {
        let operation = &stored.operation;
        for branch in &operation.branches {
            let Some(Some(repo)) = branchyard_support::best_effort(
                "resolve a running operation's shutdown target",
                self.shutdown_repo(stored, branch)
                    .map_err(|error| error.body.message),
            ) else {
                continue;
            };
            match repo.yard.cancel_as(branch, SHUTDOWN) {
                // Not created yet, or already gone: nothing to stop.
                Err(branchyard::Error::UnknownBranch(_)) => {}
                result => {
                    branchyard_support::best_effort(
                        &format!("stop branch {branch} for the shutdown"),
                        result,
                    );
                }
            }
        }
    }

    fn execute(&self, stored: &StoredOperation, work: &Value) -> Finished {
        let app = &self.0;
        let operation = &stored.operation;
        if let Err(refused) = admitted_principal_allowed(&app.config, stored)
            .and_then(|()| fresh_groups_allowed(&app.credentials, stored))
        {
            return Finished {
                result: Err(*refused.body),
                end_cursor: None,
                projection: Projection::default(),
                projection_tasks: Default::default(),
            };
        }
        if let Ok(Work::Console {
            command,
            credential,
        }) = serde_json::from_value::<Work>(work.clone())
        {
            let Some(principal) = stored.principal.clone() else {
                return Finished {
                    result: Err(*ApiError::unauthorized().body),
                    end_cursor: None,
                    projection: Projection::default(),
                    projection_tasks: Default::default(),
                };
            };
            let caller = crate::api::Caller::authenticated(principal, credential);
            if let Err(error) = app.check_caller(&caller) {
                return Finished {
                    result: Err(*error.body),
                    end_cursor: None,
                    projection: Projection::default(),
                    projection_tasks: Default::default(),
                };
            }
            return crate::console::execute(app, stored, command, caller);
        }
        // Membership may have changed while this operation waited in the queue.
        // The durable admitting principal, never the worker identity, must still
        // be allowed to act on every workspace-bound resource before any effect.
        let principal = stored.principal.clone().unwrap_or_else(|| {
            crate::config::Principal::default_for("unattributed-legacy-operation")
        });
        if let Err(refused) = crate::console::auth::authorize_operation(
            app,
            &crate::api::Caller::new(principal),
            operation,
            scope_for(operation.kind),
        ) {
            return Finished {
                result: Err(*refused.body),
                end_cursor: None,
                projection: Projection::default(),
                projection_tasks: Default::default(),
            };
        }
        let Some(repo) = app.repos.get(&operation.repo) else {
            // The repositories its principal may use, and no one else's.
            let served: Vec<&str> = app
                .repos
                .keys()
                .map(String::as_str)
                .filter(|served| match &stored.principal {
                    Some(principal) => {
                        let policy = app.config.tenant_policy(&principal.tenant);
                        principal.repo_allowed(&policy, served)
                    }
                    None => true,
                })
                .collect();
            return Finished {
                result: Err(*crate::api::unknown_repo("worker", &operation.repo, &served).body),
                end_cursor: None,
                projection: Projection::default(),
                projection_tasks: Default::default(),
            };
        };
        // A branch name and worktree path can be reused. Bind its projection
        // to the native task identity created by this operation, including a
        // failed run that returns no OperationResult.
        let task_ids = || -> Result<std::collections::BTreeMap<String, String>, ErrorBody> {
            let mut ids = std::collections::BTreeMap::new();
            for name in &stored.creates {
                if let Some(task) = branchyard::tasks::task_of(repo.yard.root(), name)
                    .map_err(|error| *crate::error::sdk(&error).body)?
                {
                    ids.insert(name.clone(), task.id);
                }
            }
            Ok(ids)
        };
        let before_tasks = match task_ids() {
            Ok(ids) => ids,
            Err(error) => {
                return Finished {
                    result: Err(stored.projection.error(error)),
                    end_cursor: None,
                    projection: Projection::default(),
                    projection_tasks: Default::default(),
                }
            }
        };
        let observability = app.registry.observability();
        // What the branches had spent before, so the operation's cost is
        // what they spent during it; and the trace their activity belongs
        // to, for webhook deliveries.
        let mut before = std::collections::BTreeMap::new();
        for name in &operation.branches {
            if let Ok(branch) = repo.yard.branch(name) {
                if let Some(cost) = branch.info().cost_usd {
                    before.insert(name.clone(), cost);
                }
            }
            if let Some(trace) = &stored.trace {
                observability.branches.note(&repo.name, name, trace);
            }
        }
        // With sync: each branch's task pulled and its lease held for the
        // operation (docs/sync.md).
        let leases = match app
            .sync
            .as_ref()
            .map(|s| s.begin(&repo.name, &operation.branches))
        {
            Some(Err(refusal)) => {
                let (status, code, message) = match refusal {
                    crate::sync::Refusal::Held(m) => (StatusCode::CONFLICT, "sync_lease_held", m),
                    crate::sync::Refusal::Unavailable(m) => {
                        (StatusCode::SERVICE_UNAVAILABLE, "sync_unavailable", m)
                    }
                };
                return Finished {
                    result: Err(*ApiError::new(status, code, message).body),
                    end_cursor: None,
                    projection: Projection::default(),
                    projection_tasks: Default::default(),
                };
            }
            Some(Ok(leases)) => leases,
            None => Vec::new(),
        };
        let mut projection = stored.projection.clone();
        let run = || {
            serde_json::from_value::<Work>(work.clone())
                .map_err(|e| {
                    *ApiError::internal(format!("unreadable operation description: {e}")).body
                })
                .and_then(|work| {
                    work.run(
                        app,
                        repo,
                        stored.principal.as_ref(),
                        stored.trace.as_deref(),
                        operation.created_at_ms,
                        &mut projection,
                    )
                })
        };
        let (result, lost) = run_under_leases(app, repo, &operation.branches, &leases, run);
        // A run whose lease was lost is not accepted: another runner may be
        // running the same task, and what this one did after the loss is
        // not pushed (its task is fenced).
        let mut result = match lost {
            Some(reason) => Err(*ApiError::new(
                StatusCode::CONFLICT,
                "sync_lease_lost",
                format!("{reason}; the run was cancelled and its result is not accepted"),
            )
            .body),
            None => result,
        };
        if let Some(sync) = &app.sync {
            let mut branches = operation.branches.clone();
            if let Ok(done) = &result {
                for b in &done.branches {
                    if !branches.contains(&b.name) {
                        branches.push(b.name.clone());
                    }
                }
            }
            sync.end(&repo.name, &branches);
        }
        drop(leases);
        // Read the feed's head so the end cursor covers all the activity.
        let end_cursor = match repo.feed.sync() {
            Ok(head) => Some(head),
            Err(e) => {
                eprintln!("branchyard-server: could not read the event feed: {e}");
                None
            }
        };
        let projection_tasks = match task_ids() {
            Ok(mut ids) => {
                ids.retain(|name, id| before_tasks.get(name) != Some(id));
                ids
            }
            Err(error) => {
                result = Err(error);
                Default::default()
            }
        };
        observe_run(app, repo, stored, &before, &result, end_cursor);
        Finished {
            result: result.map_err(|error| projection.error(error)),
            end_cursor,
            projection,
            projection_tasks,
        }
    }
}

/// How often a run checks its sync leases while it runs.
const LEASE_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Run `run` while watching `leases`: once one is lost, its task is fenced
/// (the replicator stops pushing it) and every running turn on `branches`
/// is cancelled, naming the loss, and cancelled again each poll while the
/// run lasts, so a turn starting after the loss stops too. Returns the
/// run's result and, when a lease was lost, why.
pub(crate) fn run_under_leases<T>(
    app: &App,
    repo: &RepoState,
    branches: &[String],
    leases: &[crate::sync::HeldLease],
    run: impl FnOnce() -> T,
) -> (T, Option<String>) {
    let (Some(sync), false) = (app.sync.as_ref(), leases.is_empty()) else {
        return (run(), None);
    };
    let lost: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let (done, finished) = std::sync::mpsc::channel::<()>();
    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            // Owned here: the receiver is not shared between threads.
            let finished = finished;
            let mut fenced = std::collections::BTreeSet::new();
            loop {
                for held in leases {
                    let Some(reason) = held.keeper.lost_reason() else {
                        continue;
                    };
                    if fenced.insert(held.task.clone()) {
                        let reason = format!("sync: {reason}");
                        tracing::warn!(repo = %repo.name, task = %held.task, %reason, "a sync lease was lost; stopping the run");
                        sync.fence(&repo.name, &held.task, &reason);
                        lost.lock_recovering("lost")
                            .get_or_insert(reason);
                    }
                }
                let reason = lost.lock_recovering("lost").clone();
                if let Some(reason) = reason {
                    for branch in branches.iter().chain(leases.iter().map(|h| &h.branch)) {
                        match repo.yard.cancel_as(branch, &reason) {
                            // Already gone: nothing left to stop.
                            Err(branchyard::Error::UnknownBranch(_)) => {}
                            result => {
                                branchyard_support::best_effort_once(
                                    &format!("stop branch {branch} after a lost sync lease"),
                                    result,
                                );
                            }
                        }
                    }
                }
                match finished.recv_timeout(LEASE_POLL) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    _ => return,
                }
            }
        });
        let result = run();
        drop(done);
        result
    });
    // A loss the watcher had not seen yet still counts.
    let mut lost = lost.into_inner_recovering("lost");
    for held in leases {
        if let Some(reason) = held.keeper.lost_reason() {
            let reason = format!("sync: {reason}");
            sync.fence(&repo.name, &held.task, &reason);
            lost.get_or_insert(reason);
        }
    }
    (result, lost)
}

/// Most feed entries an operation's metrics and spans are read from.
const OBSERVED_ENTRIES: usize = 100_000;

/// Count and trace what an operation did: its branches' cost, and the
/// turns, tool calls and connector calls its events record between its
/// admission and its end (see [`crate::observe`]).
fn observe_run(
    app: &App,
    repo: &RepoState,
    stored: &StoredOperation,
    before: &std::collections::BTreeMap<String, f64>,
    result: &Result<OperationResult, ErrorBody>,
    end_cursor: Option<u64>,
) {
    let observability = app.registry.observability();
    let operation = &stored.operation;
    let mut infos: Vec<branchyard::BranchInfo> = Vec::new();
    if let Ok(result) = result {
        infos.extend(result.branches.iter().cloned());
        infos.extend(result.descendants.iter().cloned());
    }
    for name in &operation.branches {
        if !infos.iter().any(|i| &i.name == name) {
            if let Ok(branch) = repo.yard.branch(name) {
                infos.push(branch.info().clone());
            }
        }
    }
    crate::observe::record_cost(&observability.metrics, stored, before, &infos);
    let Some(end) = end_cursor else { return };
    let branches: std::collections::BTreeSet<String> = infos
        .iter()
        .map(|i| i.name.clone())
        .chain(operation.branches.iter().cloned())
        .collect();
    let mut entries = Vec::new();
    let mut cursor = operation.cursor;
    while cursor < end && entries.len() < OBSERVED_ENTRIES {
        match repo.feed.read_after(cursor, 1000) {
            Ok(page) if page.is_empty() => break,
            Ok(page) => {
                cursor = page.last().map_or(end, |e| e.seq);
                entries.extend(page.into_iter().filter(|e| e.seq <= end));
            }
            Err(e) => {
                tracing::warn!(error = %e, "reading an operation's events for its metrics");
                return;
            }
        }
    }
    let harness: std::collections::BTreeMap<String, String> = infos
        .iter()
        .map(|i| (i.name.clone(), i.harness.clone()))
        .collect();
    let harness_of = |b: &str| harness.get(b).cloned().unwrap_or_else(|| "unknown".into());
    crate::observe::record_events(
        observability,
        &branches,
        &harness_of,
        &entries,
        stored.trace.as_deref(),
    );
    if operation.kind == OperationKind::Task {
        let created = operation.branches.iter().cloned().collect();
        crate::observe::record_starts(
            &observability.metrics,
            &repo.name,
            operation.created_at_ms,
            &created,
            &entries,
        );
    }
}

/// The scope an operation of `kind` needs, as its endpoint checks it.
pub(crate) fn scope_for(kind: OperationKind) -> &'static str {
    match kind {
        OperationKind::Merge | OperationKind::Integrate => "merge",
        _ => "run",
    }
}

/// Refuse to run an operation whose admitting principal this worker's
/// configuration would not let act on its repository, with the error the
/// request would have got here. The worker holds no credential of its
/// own: it acts as the recorded principal. A record from before tenants
/// existed was admitted with every scope, in the default tenant.
fn admitted_principal_allowed(
    config: &crate::config::Config,
    stored: &StoredOperation,
) -> Result<(), ApiError> {
    let Some(principal) = &stored.principal else {
        return Ok(());
    };
    crate::config::check_principal(principal)
        .map_err(|why| ApiError::new(StatusCode::FORBIDDEN, "principal_invalid", why))?;
    if principal.origin != crate::config::AuthenticationOrigin::Configured
        && !crate::auth::origin_allowed(config, principal)
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "identity_binding_revoked",
            "the admitted identity's issuing policy changed or was removed",
        ));
    }
    let repo = &stored.operation.repo;
    let scope = scope_for(stored.operation.kind);
    if !principal.allows(scope) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "scope_required",
            format!("this operation's principal does not hold the {scope} scope"),
        )
        .detail(serde_json::json!({ "scope": scope })));
    }
    let policy = config.tenant_policy(&principal.tenant);
    if !principal.repo_allowed(&policy, repo) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "repo_not_allowed",
            format!("this operation's principal may not act on repository {repo}"),
        )
        .detail(serde_json::json!({ "repo": repo })));
    }
    Ok(())
}

/// Refuse to run an operation whose principal's groups, read again, no
/// longer admit it. Groups a federated sign-in read from Cloud Identity are
/// looked up again here once they are older than the lookup's cache, and the
/// binding is chosen with the fresh groups, so a person removed from a group
/// loses its access without signing in again. A lookup that fails refuses
/// as `identity_unavailable`: the groups are never guessed. Blocking, which
/// an executor is allowed to be. See `docs/identity.md`.
fn fresh_groups_allowed(
    credentials: &crate::auth::Credentials,
    stored: &StoredOperation,
) -> Result<(), ApiError> {
    let Some(principal) = &stored.principal else {
        return Ok(());
    };
    match credentials.recheck_groups(principal, branchyard_support::time::now_ms()) {
        Ok(true) => Ok(()),
        Ok(false) => Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "identity_binding_revoked",
            "the admitted identity's groups changed and its binding no longer allows this operation",
        )),
        Err(refusal) => Err(refusal.into()),
    }
}

/// The branches an operation ran, once every branch they delegated to on
/// this server has finished, as `by run` waits for them. Each is read back
/// as `GET …/branches/{b}` reads it, with its `awaiting` and `stage`, which
/// a run's own handle does not carry.
pub(crate) fn finished(branches: Vec<Branch>) -> Result<OperationResult, branchyard::Error> {
    let mut descendants = Vec::new();
    for branch in &branches {
        descendants.extend(branch.wait_subtree()?);
    }
    let branches = branches
        .iter()
        .map(|b| Ok(b.yard().branch(&b.info().name)?.info().clone()))
        .collect::<Result<Vec<_>, branchyard::Error>>()?;
    Ok(OperationResult {
        branches,
        descendants,
        ..OperationResult::default()
    })
}

/// The map a request describes, checked as running it would check it, or
/// why this server refuses it. Its `launch` is the request without its
/// items (the spec keeps them), for `.../maps/{name}/resume`.
pub(crate) fn map_spec(request: &MapRequest) -> Result<branchyard::MapSpec, ApiError> {
    let task = &request.task;
    for (given, what) in [
        (task.name.is_some(), "task.name (the map's name is name)"),
        (
            !task.harnesses.is_empty(),
            "task.harnesses (a map runs each item on one branch)",
        ),
        (task.seats.is_some(), "task.seats"),
        (task.plan, "task.plan"),
        (task.goal.is_some(), "task.goal"),
    ] {
        if given {
            return Err(ApiError::bad_request(format!("a map takes no {what}")));
        }
    }
    if request.items.is_empty() {
        return Err(ApiError::bad_request("the map has no items"));
    }
    let name = request
        .name
        .clone()
        .unwrap_or_else(|| branchyard::map_default_name(&task.prompt));
    let mut spec = branchyard::MapSpec::new(name, task.prompt.clone(), request.items.clone());
    spec.schema = request.schema.clone();
    spec.concurrency = request
        .concurrency
        .unwrap_or(branchyard::MAP_DEFAULT_CONCURRENCY);
    spec.retries = request.retries.unwrap_or(branchyard::MAP_DEFAULT_RETRIES);
    spec.total_usd = request.total_usd;
    spec.reduce = request.reduce.clone();
    spec.remove_done = request.remove_done;
    branchyard::check_map_spec(&spec).map_err(|e| crate::error::sdk(&e))?;
    let launch = MapRequest {
        items: Vec::new(),
        retry_failed: false,
        ..request.clone()
    };
    spec.launch = serde_json::to_value(&launch)
        .map_err(|e| ApiError::internal(format!("could not record the map's request: {e}")))?;
    Ok(spec)
}

/// A task's options, or why this server refuses it.
pub(crate) fn task_options(
    app: &App,
    repo: &RepoState,
    request: &TaskRequest,
) -> Result<TaskOptions, ApiError> {
    // A preset is resolved for a tenant: through `with_preset`, never here.
    if request.preset.is_some() {
        return Err(ApiError::bad_request(
            "an execution preset runs a task or a fan (POST .../tasks); not here yet",
        ));
    }
    task_options_from(app, repo, request, None)
}

/// A task request as it runs, with its execution preset resolved for
/// `tenant` (`None`: a request without a preset passes unchanged, and one
/// with a preset is refused), and its options.
pub(crate) fn preset_or_task_options(
    app: &App,
    repo: &RepoState,
    request: &TaskRequest,
    tenant: Option<&str>,
) -> Result<(TaskRequest, TaskOptions, Projection), ApiError> {
    match (&request.preset, tenant) {
        (Some(_), Some(tenant)) => {
            let (resolved, preset) = crate::console::for_task(app, tenant, request)?;
            let projection = Projection::of_preset(&preset);
            let options = task_options_from(app, repo, &resolved, Some(&preset))
                .map_err(|error| projection.api_error(error))?;
            Ok((resolved, options, projection))
        }
        _ => Ok((
            request.clone(),
            task_options(app, repo, request)?,
            Projection::default(),
        )),
    }
}

/// Build a console turn using only a server-resolved execution preset.
pub(crate) fn preset_task_options(
    app: &App,
    repo: &RepoState,
    request: &TaskRequest,
    preset: &crate::console::ResolvedPreset,
) -> Result<TaskOptions, ApiError> {
    task_options_from(app, repo, request, Some(preset))
}

fn task_options_from(
    app: &App,
    repo: &RepoState,
    request: &TaskRequest,
    preset: Option<&crate::console::ResolvedPreset>,
) -> Result<TaskOptions, ApiError> {
    if request.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt is empty"));
    }
    if request.harness.is_some() && !request.harnesses.is_empty() {
        return Err(ApiError::bad_request(
            "give harness for one branch or harnesses for several, not both",
        ));
    }
    let budget = request.budget.to_budget().map_err(ApiError::bad_request)?;
    if let Some(contract) = &request.result {
        contract
            .check()
            .map_err(|error| ApiError::bad_request(error.to_string()))?;
        if contract.is_deliverable() && request.goal.is_some() {
            return Err(ApiError::bad_request(
                "a result deliverable does not run automatic goal follow-up turns",
            ));
        }
    }
    let provider = match preset {
        Some(preset) => preset.provider(app, request.provider.as_ref())?,
        None => app.provider(request.provider.clone())?,
    };
    app.opt_ins(
        request.delegation.is_some() || request.seats.is_some(),
        request.allow_delegation,
        request.unapproved_tools,
    )?;
    if let Some(seats) = &request.seats {
        if request.delegation.is_none() {
            return Err(ApiError::bad_request(
                "seats need a delegation envelope to spawn them within",
            ));
        }
        seats
            .validate()
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
    }
    let targets: Vec<Option<&str>> = match request.harnesses.is_empty() {
        true => vec![request.harness.as_deref()],
        false => request.harnesses.iter().map(|h| Some(h.as_str())).collect(),
    };
    let command = app.command(request.command.clone(), &targets)?;
    Ok(TaskOptions {
        harness: request.harness.clone(),
        name: request.name.clone(),
        base: request.base.clone(),
        isolated: request.isolated,
        provision: app.provision(request.provision.clone())?,
        seats: app.seats(request.seats.clone())?,
        workspace: app.workspace(repo)?,
        plan: request.plan,
        goal: goal(app, request)?,
        result: request.result.clone(),
        deny: request.deny.clone(),
        ..app.options(
            repo,
            budget,
            app.policy(&request.policy, request.allow_delegation),
            request.check.clone(),
            command,
            request.delegation.clone(),
            request.unapproved_tools,
            provider,
        )
    })
}

/// Who a plan decision names: the request's principal, through the server.
fn person(principal: Option<&crate::config::Principal>) -> String {
    match principal {
        Some(p) => crate::api::through_the_server(&p.name),
        None => "a person through the server".into(),
    }
}

/// The options of a plan approval's or re-plan's turn: a send's, whose
/// prompt the plan replaces.
pub(crate) fn plan_send_options(
    app: &App,
    repo: &RepoState,
    request: &SendRequest,
) -> Result<TaskOptions, ApiError> {
    let request = SendRequest {
        prompt: "plan".into(),
        ..request.clone()
    };
    send_options(app, repo, &request)
}

/// A task's goal, as the server runs it: its judge is one of the server's
/// harnesses, launched with the server's command for it.
pub(crate) fn goal(app: &App, request: &TaskRequest) -> Result<Option<branchyard::Goal>, ApiError> {
    let Some(goal) = &request.goal else {
        return Ok(None);
    };
    if goal.text.trim().is_empty() {
        return Err(ApiError::bad_request("goal.text is empty"));
    }
    let judge = match &goal.judge {
        Some(harness) => Some(branchyard::JudgeSpec {
            harness: harness.clone(),
            model: None,
            effort: None,
            command: app.command(None, &[Some(harness)])?,
            rubric: None,
        }),
        None => None,
    };
    Ok(Some(branchyard::Goal {
        text: goal.text.clone(),
        rounds: goal
            .rounds
            .unwrap_or(branchyard::GOAL_DEFAULT_ROUNDS)
            .min(20),
        judge,
        custom: None,
    }))
}

/// A send's options, or why this server refuses it. See also
/// [`send_allowed`], which needs the branch.
pub(crate) fn send_options(
    app: &App,
    repo: &RepoState,
    request: &SendRequest,
) -> Result<TaskOptions, ApiError> {
    if let Some(patch) = &request.session_patch {
        patch.check().map_err(ApiError::bad_request)?;
        if request.provision.is_some() {
            return Err(ApiError::bad_request(
                "session_patch and provision are mutually exclusive",
            ));
        }
    }
    match (request.retry, request.prompt.trim().is_empty()) {
        (true, false) => {
            return Err(ApiError::bad_request(
                "give a prompt or retry, not both: retry sends the cut-off turn's prompt again",
            ))
        }
        (false, true) => return Err(ApiError::bad_request("prompt is empty")),
        _ => {}
    }
    if request.retry && request.feedback.is_some() {
        return Err(ApiError::bad_request(
            "feedback describes the prompt given with it, and retry gives none",
        ));
    }
    let budget = request.budget.to_budget().map_err(ApiError::bad_request)?;
    // A send keeps the branch's recorded command unless the request
    // replaces it.
    let command = match request.command.clone() {
        Some(command) => app.command(Some(command), &[])?,
        None => None,
    };
    app.opt_ins(
        request.delegation.is_some(),
        request.allow_delegation,
        request.unapproved_tools,
    )?;
    Ok(TaskOptions {
        provision: app.provision(request.provision.clone())?,
        session_patch: request.session_patch.clone(),
        ..app.options(
            repo,
            budget,
            app.policy(&request.policy, request.allow_delegation),
            request.check.clone(),
            command,
            request.delegation.clone(),
            request.unapproved_tools,
            None,
        )
    })
}

/// The prompt a send submits: its own, or with `retry` that of the
/// branch's last turn that was cut off. Blocks: it reads the branch.
pub(crate) fn sent_prompt(target: &Branch, request: &SendRequest) -> Result<String, ApiError> {
    match request.retry {
        true => target.retry_prompt().map_err(|e| error::sdk(&e)),
        false => Ok(request.prompt.clone()),
    }
}

/// Record a send's feedback, when it has some, on `target`'s log as
/// carried by `prompt`, just before the turn. Blocks.
fn record_feedback(
    target: &Branch,
    prompt: &str,
    request: &SendRequest,
) -> Result<(), branchyard::Error> {
    match &request.feedback {
        Some(feedback) => target.record_feedback(prompt, feedback.clone()),
        None => Ok(()),
    }
}

/// Refuse a send that keeps a delegation envelope this server does not
/// offer. Blocks: it reads the branch's envelope.
pub(crate) fn send_allowed(
    app: &App,
    target: &Branch,
    branch: &str,
    request: &SendRequest,
) -> Result<(), ApiError> {
    send_allowed_from(app, target, branch, request, None)
}

/// Recheck the private preset and actual branch provider before continuation.
pub(crate) fn preset_send_allowed(
    app: &App,
    target: &Branch,
    branch: &str,
    request: &SendRequest,
    preset: &crate::console::ResolvedPreset,
) -> Result<(), ApiError> {
    send_allowed_from(app, target, branch, request, Some(preset))
}

fn send_allowed_from(
    app: &App,
    target: &Branch,
    branch: &str,
    request: &SendRequest,
    preset: Option<&crate::console::ResolvedPreset>,
) -> Result<(), ApiError> {
    let provider = target.provider().map_err(|error| error::sdk(&error))?;
    match preset {
        Some(preset) => {
            preset.provider(app, provider.as_ref())?;
        }
        None => {
            app.provider(provider)?;
        }
    }
    if app.config.allow_delegation || request.delegation.is_some() {
        return Ok(());
    }
    // A send keeps the branch's envelope; this server offers none.
    let envelope = target
        .delegate(TaskOptions::default())
        .and_then(|d| d.inspect(&target.info().name))
        .map(|i| i.envelope)
        .map_err(|e| error::sdk(&e))?;
    if envelope.is_some_and(|e| e.max_depth > 0) {
        return Err(crate::api::delegation_not_allowed(&format!(
            "{branch} was given delegation, and this server does not offer delegation to \
             its harnesses; its operator can allow it with --allow-delegation"
        )));
    }
    Ok(())
}

/// The command a fork or reincarnation runs: without a harness it keeps
/// its parent's, and its command.
fn inherited_command(
    app: &App,
    command: &Option<Vec<String>>,
    harness: &Option<String>,
) -> Result<Option<Vec<String>>, ApiError> {
    match (command, harness) {
        (Some(_), _) => app.command(command.clone(), &[]),
        (None, Some(harness)) => app.command(None, &[Some(harness)]),
        (None, None) => Ok(None),
    }
}

/// A fork's options, or why this server refuses it.
pub(crate) fn fork_options(
    app: &App,
    repo: &RepoState,
    request: &ForkRequest,
) -> Result<TaskOptions, ApiError> {
    if request.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt is empty"));
    }
    if request.at.is_some() && request.fresh_session {
        return Err(ApiError::bad_request(
            "at and fresh_session cannot be combined: a fork at a checkpoint starts a fresh \
             session itself when the harness's cannot be forked there",
        ));
    }
    let budget = request.budget.to_budget().map_err(ApiError::bad_request)?;
    let provider = app.provider(request.provider.clone())?;
    app.opt_ins(
        request.delegation.is_some(),
        request.allow_delegation,
        request.unapproved_tools,
    )?;
    let command = inherited_command(app, &request.command, &request.harness)?;
    Ok(TaskOptions {
        harness: request.harness.clone(),
        name: request.name.clone(),
        isolated: request.isolated,
        provision: app.provision(request.provision.clone())?,
        workspace: app.workspace(repo)?,
        ..app.options(
            repo,
            budget,
            app.policy(&request.policy, request.allow_delegation),
            request.check.clone(),
            command,
            request.delegation.clone(),
            request.unapproved_tools,
            provider,
        )
    })
}

/// A reincarnation's options, or why this server refuses it.
pub(crate) fn reincarnate_options(
    app: &App,
    repo: &RepoState,
    request: &ReincarnateRequest,
) -> Result<TaskOptions, ApiError> {
    let budget = request.budget.to_budget().map_err(ApiError::bad_request)?;
    let provider = app.provider(request.provider.clone())?;
    app.opt_ins(
        request.delegation.is_some(),
        request.allow_delegation,
        request.unapproved_tools,
    )?;
    let command = inherited_command(app, &request.command, &request.harness)?;
    Ok(TaskOptions {
        harness: request.harness.clone(),
        name: request.name.clone(),
        isolated: request.isolated,
        provision: app.provision(request.provision.clone())?,
        workspace: app.workspace(repo)?,
        ..app.options(
            repo,
            budget,
            app.policy(&request.policy, request.allow_delegation),
            request.check.clone(),
            command,
            request.delegation.clone(),
            request.unapproved_tools,
            provider,
        )
    })
}

/// A spawn's options and child, named `name` when admission fixed it, or
/// why this server refuses it.
pub(crate) fn spawn_parts(
    app: &App,
    repo: &RepoState,
    request: &SpawnRequest,
    name: Option<String>,
) -> Result<(TaskOptions, Spawn), ApiError> {
    if !app.config.allow_delegation {
        return Err(crate::api::delegation_not_allowed(
            "this server does not offer delegation, so it does not spawn children; its \
             operator can allow it with --allow-delegation",
        ));
    }
    app.opt_ins(false, false, request.unapproved_tools)?;
    if request.prompt.trim().is_empty() {
        return Err(error::sdk(&branchyard::Error::Denied(
            "a child needs a prompt".into(),
        )));
    }
    // A contract outside the subset is refused now, not by the worker.
    if let Some(result) = &request.result {
        result.check().map_err(|e| error::sdk(&e))?;
    }
    let budget = request.budget.to_budget().map_err(ApiError::bad_request)?;
    let options = app.options(
        repo,
        budget.clone(),
        app.policy(&request.policy, false),
        request.check.clone(),
        None,
        None,
        request.unapproved_tools,
        None,
    );
    let spawn = Spawn {
        prompt: request.prompt.clone(),
        harness: request.harness.clone(),
        name,
        base: request.base.clone(),
        budget,
        check: request.check.clone(),
        max_depth: request.max_depth,
        deny: request.deny.clone(),
        seat: request.seat.clone(),
        depends_on: request.depends_on.clone(),
        after: request.after,
        bindings: request.bindings.clone(),
        connectors: request.connectors.clone(),
        max_children: request.max_children,
        harnesses: request.harnesses.clone(),
        plan: request.plan,
        model: request.model.clone(),
        result: request.result.clone(),
        why: request.why.clone(),
        owns: request.owns.clone(),
        land: request.land,
        review: request.review.clone(),
    };
    Ok((options, spawn))
}

/// The branch that delegated `name` and still lists it as a child.
pub(crate) fn delegator(yard: &Yard, name: &str) -> Result<String, branchyard::Error> {
    let info = yard.branch(name)?.info().clone();
    info.parent
        .filter(|p| {
            yard.branch(p)
                .is_ok_and(|p| p.info().children.iter().any(|c| c == name))
        })
        .ok_or_else(|| {
            branchyard::Error::Denied(format!(
                "{name} was not delegated by another branch; merge it with by merge"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A queued operation of a person who signed in through IAP with groups
    /// read from Cloud Identity at `groups_at_ms`, and the credentials of a
    /// server whose Cloud Identity is `identity`.
    fn queued_federated_work(
        identity: &branchyard_testkit::MockHttp,
        groups_at_ms: Option<u64>,
    ) -> (crate::auth::Credentials, StoredOperation) {
        use crate::config::{AuthenticationOrigin, Principal};
        use crate::federated::{AccessToken, GroupsFrom, IapConfig, IdentityBinding};
        struct Token;
        impl AccessToken for Token {
            fn token(&self) -> Result<String, String> {
                Ok("meta-token".into())
            }
        }
        let binding = |matches: &str, name: &str, scopes: &[&str]| IdentityBinding {
            matches: matches.into(),
            name: Some(name.into()),
            tenant: Some("engineering".into()),
            scopes: Some(scopes.iter().map(|s| (*s).to_owned()).collect()),
            repos: None,
        };
        let iap = IapConfig {
            audience: "/projects/1/global/backendServices/2".into(),
            keys_url: None,
            tenant: None,
            scopes: None,
            bindings: vec![
                binding("group:eng@example.com", "engineer", &["read", "run"]),
                binding("domain:example.com", "staff", &["read"]),
            ],
            groups_from: Some(GroupsFrom::CloudIdentity),
            groups_url: Some(identity.url()),
        };
        let credentials = crate::auth::Credentials::new(Vec::new()).with_federation(
            Some(iap),
            Vec::new(),
            Arc::new(Token),
        );
        let principal = Principal {
            origin: AuthenticationOrigin::Iap {
                email: "ada@example.com".into(),
                sub: "accounts.google.com:1".into(),
                groups_at_ms,
            },
            name: "engineer".into(),
            tenant: "engineering".into(),
            scopes: ["read".to_owned(), "run".to_owned()].into_iter().collect(),
            repos: None,
            groups: ["eng@example.com".to_owned()].into_iter().collect(),
        };
        let stored: StoredOperation = serde_json::from_value(serde_json::json!({
            "operation":{"id":"queued","repo":"app","kind":"task","state":"queued","branches":[],"cursor":0,"created_at_ms":0},
            "principal":principal,"tenant":"engineering"
        })).unwrap();
        (credentials, stored)
    }

    #[test]
    fn queued_work_rereads_old_groups_and_refuses_a_person_removed_from_one() {
        use branchyard_testkit::{MockHttp, Response};
        let old = Some(branchyard_support::time::now_ms().saturating_sub(10 * 60 * 1000));
        let member = |present: bool| {
            MockHttp::start(move |_| match present {
                true => Response::json(
                    200,
                    &serde_json::json!({"memberships":[{"groupKey":{"id":"eng@example.com"}}]}),
                ),
                false => Response::json(200, &serde_json::json!({})),
            })
        };
        // Still in the group: it runs, after one lookup.
        let identity = member(true);
        let (credentials, stored) = queued_federated_work(&identity, old);
        assert!(fresh_groups_allowed(&credentials, &stored).is_ok());
        assert_eq!(identity.requests().len(), 1);
        // Removed from it: the binding found with the fresh groups allows
        // less than the person was admitted with. No new sign-in needed.
        let identity = member(false);
        let (credentials, stored) = queued_federated_work(&identity, old);
        let refused = fresh_groups_allowed(&credentials, &stored).unwrap_err();
        assert_eq!(refused.body.code, "identity_binding_revoked");
        // A lookup that fails is a refusal, never a guess.
        let down = MockHttp::start(|_| Response::new(503, "down"));
        let (credentials, stored) = queued_federated_work(&down, old);
        let refused = fresh_groups_allowed(&credentials, &stored).unwrap_err();
        assert_eq!(refused.body.code, "identity_unavailable");
        assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
        // Groups younger than the lookup's cache are not read again.
        let identity = member(false);
        let (credentials, stored) =
            queued_federated_work(&identity, Some(branchyard_support::time::now_ms()));
        assert!(fresh_groups_allowed(&credentials, &stored).is_ok());
        assert!(identity.requests().is_empty());
        // A principal with no federated provider is left to the ordinary
        // recheck.
        let (credentials, _) = queued_federated_work(&identity, old);
        let plain: StoredOperation = serde_json::from_value(serde_json::json!({
            "operation":{"id":"q","repo":"app","kind":"task","state":"queued","branches":[],"cursor":0,"created_at_ms":0},
            "principal":crate::config::Principal::default_for("static"),"tenant":"default"
        })).unwrap();
        assert!(fresh_groups_allowed(&credentials, &plain).is_ok());
    }

    #[test]
    fn queued_paired_google_work_cannot_outlive_its_issuer_binding() {
        use crate::config::{
            AuthenticationOrigin, Config, GoogleIdTokenConfig, GoogleIdentityBinding, Principal,
        };
        let mut issuer = Principal::default_for("operator-display");
        issuer.repos = Some(["app".to_owned(), "other".to_owned()].into_iter().collect());
        let mut config = Config::new(std::path::PathBuf::new());
        config.google_identity = Some(GoogleIdTokenConfig {
            audience: "server".into(),
            bindings: vec![GoogleIdentityBinding {
                issuer: "https://accounts.google.com".into(),
                subject: "paired-external".into(),
                principal: issuer.clone(),
            }],
        });
        issuer.origin = AuthenticationOrigin::GoogleIdToken {
            issuer: "https://accounts.google.com".into(),
            subject: "paired-external".into(),
            audience: "server".into(),
        };
        let mut phone = issuer.delegated_to("phone".into());
        phone.scopes = ["run".into()].into_iter().collect();
        phone.repos = Some(["app".to_owned()].into_iter().collect());
        let stored: StoredOperation = serde_json::from_value(serde_json::json!({"operation":{"id":"paired-queued","repo":"app","kind":"task","state":"queued","branches":[],"cursor":0,"created_at_ms":0},"principal":phone.delegated_to("tablet".into()),"tenant":"default"})).unwrap();
        assert!(admitted_principal_allowed(&config, &stored).is_ok());
        config.google_identity = None;
        assert_eq!(
            admitted_principal_allowed(&config, &stored)
                .unwrap_err()
                .body
                .code,
            "identity_binding_revoked"
        );
    }

    #[test]
    fn queued_external_work_rechecks_binding_after_durable_round_trip() {
        use crate::config::{
            AuthenticationOrigin, Config, GoogleIdTokenConfig, GoogleIdentityBinding, Principal,
        };
        let mut principal = Principal::default_for("display");
        let binding = GoogleIdentityBinding {
            issuer: "https://accounts.google.com".into(),
            subject: "external-subject".into(),
            principal: principal.clone(),
        };
        let mut config = Config::new(std::path::PathBuf::new());
        config.google_identity = Some(GoogleIdTokenConfig {
            audience: "server".into(),
            bindings: vec![binding],
        });
        principal.origin = AuthenticationOrigin::GoogleIdToken {
            issuer: "https://accounts.google.com".into(),
            subject: "external-subject".into(),
            audience: "server".into(),
        };
        let stored: StoredOperation = serde_json::from_value(serde_json::json!({
            "operation":{"id":"queued", "repo":"app", "kind":"task", "state":"queued", "branches":[], "cursor":0, "created_at_ms":0},
            "principal":principal, "tenant":"default"
        })).unwrap();
        let durable: StoredOperation =
            serde_json::from_str(&serde_json::to_string(&stored).unwrap()).unwrap();
        assert!(admitted_principal_allowed(&config, &durable).is_ok());
        let policies = [("display".to_owned(), "bounded".to_owned())]
            .into_iter()
            .collect();
        let expanded = config.actor_policy(&policies);
        assert_eq!(expanded[&principal.connector_subject()], "bounded");
        config.google_identity.as_mut().unwrap().bindings[0]
            .principal
            .scopes
            .remove("run");
        assert_eq!(
            admitted_principal_allowed(&config, &durable)
                .unwrap_err()
                .body
                .code,
            "identity_binding_revoked"
        );
        config.google_identity = None;
        assert_eq!(
            admitted_principal_allowed(&config, &durable)
                .unwrap_err()
                .body
                .code,
            "identity_binding_revoked"
        );
    }

    #[test]
    fn descriptions_round_trip_and_name_their_kind() {
        let works = [
            Work::Task {
                request: TaskRequest {
                    prompt: "p".into(),
                    ..TaskRequest::default()
                },
                trigger: None,
            },
            Work::Task {
                request: TaskRequest {
                    prompt: "p".into(),
                    ..TaskRequest::default()
                },
                trigger: Some(branchyard::tasks::TaskTrigger {
                    id: "trg_1".into(),
                    name: "triage".into(),
                    run: "run_1".into(),
                    actor: Some(branchyard::tasks::TriggerActor {
                        source: "github".into(),
                        name: "alice".into(),
                        bot: false,
                    }),
                }),
            },
            Work::Send {
                branch: "b".into(),
                request: SendRequest::default(),
            },
            Work::Fork {
                branch: "b".into(),
                request: ForkRequest::default(),
            },
            Work::Reincarnate {
                branch: "b".into(),
                request: ReincarnateRequest::default(),
            },
            Work::Merge {
                branch: "b".into(),
                target: "main".into(),
            },
            Work::Spawn {
                parent: "b".into(),
                name: Some("b-c".into()),
                request: SpawnRequest::default(),
            },
            // A spawn that waits: its dependencies and bindings are part of
            // the durable description a worker reads.
            Work::Spawn {
                parent: "b".into(),
                name: Some("b-d".into()),
                request: SpawnRequest {
                    prompt: "p".into(),
                    depends_on: vec!["b-c".into()],
                    after: branchyard::After::Integrated,
                    bindings: vec![branchyard::Binding {
                        scratch: "notes".into(),
                        access: branchyard::Access::ExclusiveWrite,
                    }],
                    ..SpawnRequest::default()
                },
            },
            Work::Integrate {
                branch: "b-c".into(),
                parent: "b".into(),
                with: Vec::new(),
            },
            Work::Integrate {
                branch: "b-c".into(),
                parent: "b".into(),
                with: vec!["b-d".into()],
            },
            Work::Check { branch: "b".into() },
        ];
        for work in works {
            let value = work.to_value().unwrap();
            let kind = serde_json::to_value(work.kind()).unwrap();
            assert_eq!(value["kind"], kind);
            assert_eq!(serde_json::from_value::<Work>(value).unwrap(), work);
        }
    }

    #[test]
    fn a_spawn_description_carries_what_the_child_waits_for() {
        let value = serde_json::json!({
            "kind": "spawn",
            "parent": "root",
            "name": "app",
            "request": {"prompt": "p", "depends_on": ["lib"], "after": "integrated",
                        "bindings": [{"scratch": "notes", "access": "read_only"}]},
        });
        let Work::Spawn { request, .. } = serde_json::from_value::<Work>(value).unwrap() else {
            panic!("not a spawn");
        };
        assert_eq!(request.depends_on, ["lib"]);
        assert_eq!(request.after, branchyard::After::Integrated);
        assert_eq!(request.bindings[0].access, branchyard::Access::ReadOnly);
    }
}

#[cfg(test)]
mod captured_credentials {
    use super::*;

    #[test]
    fn console_queue_round_trip_retains_exact_credential_without_changing_subject() {
        let principal = crate::config::Principal::default_for("alice");
        let verified = crate::companion::Verified {
            token_sha256: crate::config::sha256_hex(b"inert-test-credential"),
            expires_at_ms: Some(123_456),
        };
        let caller = crate::api::Caller::authenticated(principal.clone(), Some(verified.clone()));
        assert_eq!(
            caller.idempotency_scope(),
            crate::api::Caller::new(principal).idempotency_scope()
        );
        assert_eq!(caller.verified(), Some(&verified));
        let work = Work::Console {
            command: crate::console::ConsoleWork::Initialize {
                workspace_id: "workspace-test".into(),
            },
            credential: caller.verified().cloned(),
        };
        let encoded = serde_json::to_string(&work).unwrap();
        assert!(!encoded.contains("inert-test-credential"));
        assert_eq!(serde_json::from_str::<Work>(&encoded).unwrap(), work);
        let legacy = serde_json::json!({
            "kind":"console",
            "command":{"action":"initialize","workspace_id":"workspace-test"}
        });
        assert!(matches!(
            serde_json::from_value::<Work>(legacy).unwrap(),
            Work::Console {
                credential: None,
                ..
            }
        ));
    }
}
