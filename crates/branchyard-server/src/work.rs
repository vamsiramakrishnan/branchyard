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
//! [`Work::run`] is the executor: it rebuilds the engine's options with
//! the same rules the handlers check at admission, and calls the SDK.

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
use crate::error::{self, ApiError};
use crate::ops::{Executor, Finished};
use crate::store::StoredOperation;

/// Every kind of operation, as its queue row describes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Work {
    /// `POST /v1/repos/{repo}/tasks`.
    Task { request: TaskRequest },
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
    /// admission.
    Integrate { branch: String, parent: String },
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
}

impl Work {
    pub fn kind(&self) -> OperationKind {
        match self {
            Work::Task { .. } => OperationKind::Task,
            Work::Send { .. } => OperationKind::Send,
            Work::Fork { .. } => OperationKind::Fork,
            Work::Reincarnate { .. } => OperationKind::Reincarnate,
            Work::Merge { .. } => OperationKind::Merge,
            Work::Spawn { .. } => OperationKind::Spawn,
            Work::Integrate { .. } => OperationKind::Integrate,
            Work::ApprovePlan { .. } => OperationKind::ApprovePlan,
            Work::RejectPlan { .. } => OperationKind::RejectPlan,
            Work::Map { .. } => OperationKind::Map,
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
    ) -> Result<OperationResult, ErrorBody> {
        let trace_parent = trace.map(str::to_owned);
        let sdk = |e: branchyard::Error| *error::sdk(&e).body;
        let api = |e: ApiError| *e.body;
        let yard = &repo.yard;
        // A new branch acts for the principal that asked for it at the
        // connector gateway; see docs/connectors.md.
        let actor = principal.map(|p| branchyard::connectors::Actor {
            subject: p.name.clone(),
            tenant: p.tenant.clone(),
        });
        match self {
            Work::Task { request } => {
                let options = TaskOptions {
                    actor: actor.clone(),
                    trace_parent: trace_parent.clone(),
                    ..task_options(app, repo, &request).map_err(api)?
                };
                let builder = yard.task(request.prompt.clone()).options(options);
                let ran = match request.harnesses.is_empty() {
                    true => builder.run().map(|b| vec![b]),
                    false => {
                        let ids: Vec<&str> = request.harnesses.iter().map(String::as_str).collect();
                        builder.run_on(&ids)
                    }
                };
                ran.and_then(finished).map_err(sdk)
            }
            Work::Send { branch, request } => {
                let options = TaskOptions {
                    trace_parent: trace_parent.clone(),
                    ..send_options(app, repo, &request).map_err(api)?
                };
                let target = yard.branch(&branch).map_err(sdk)?;
                send_allowed(app, &target, &branch, &request).map_err(api)?;
                target
                    .send(&request.prompt, options)
                    .and_then(|b| finished(vec![b]))
                    .map_err(sdk)
            }
            Work::Fork { branch, request } => {
                let options = TaskOptions {
                    actor: actor.clone(),
                    trace_parent: trace_parent.clone(),
                    ..fork_options(app, repo, &request).map_err(api)?
                };
                yard.branch(&branch)
                    .and_then(|source| source.fork(&request.prompt, request.fresh_session, options))
                    .and_then(|b| finished(vec![b]))
                    .map_err(sdk)
            }
            Work::Reincarnate { branch, request } => {
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
            Work::Integrate { branch, parent } => {
                let run = || {
                    let options = TaskOptions {
                        observer: Some(crate::api::observer(&repo.wake)),
                        ..TaskOptions::default()
                    };
                    let merged = yard
                        .branch(&parent)?
                        .delegate(options)?
                        .integrate(&branch)?;
                    Ok::<_, branchyard::Error>(OperationResult {
                        branches: vec![yard.branch(&branch)?.info().clone()],
                        merged: Some(merged),
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

impl Executor for AppExecutor {
    fn execute(&self, stored: &StoredOperation, work: &Value) -> Finished {
        let app = &self.0;
        let operation = &stored.operation;
        if let Err(refused) = admitted_principal_allowed(app, stored) {
            return Finished {
                result: Err(*refused.body),
                end_cursor: None,
            };
        }
        let Some(repo) = app.repos.get(&operation.repo) else {
            return Finished {
                result: Err(*ApiError::new(
                    StatusCode::NOT_FOUND,
                    "unknown_repo",
                    format!("this worker serves no repository named {}", operation.repo),
                )
                .body),
                end_cursor: None,
            };
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
                observability.tracer.note_branch(&repo.name, name, trace);
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
                };
            }
            Some(Ok(leases)) => leases,
            None => Vec::new(),
        };
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
                    )
                })
        };
        let (result, lost) = run_under_leases(app, repo, &operation.branches, &leases, run);
        // A run whose lease was lost is not accepted: another runner may be
        // running the same task, and what this one did after the loss is
        // not pushed (its task is fenced).
        let result = match lost {
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
        observe_run(app, repo, stored, &before, &result, end_cursor);
        Finished { result, end_cursor }
    }
}

/// How often a run checks its sync leases while it runs.
const LEASE_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Run `run` while watching `leases`: once one is lost, its task is fenced
/// (the replicator stops pushing it) and every running turn on `branches`
/// is cancelled, naming the loss, and cancelled again each poll while the
/// run lasts, so a turn starting after the loss stops too. Returns the
/// run's result and, when a lease was lost, why.
fn run_under_leases<T>(
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
                        lost.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .get_or_insert(reason);
                    }
                }
                let reason = lost.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if let Some(reason) = reason {
                    for branch in branches.iter().chain(leases.iter().map(|h| &h.branch)) {
                        let _ = repo.yard.cancel_as(branch, &reason);
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
    let mut lost = lost.into_inner().unwrap_or_else(|e| e.into_inner());
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
                cursor = page.last().map(|e| e.seq).unwrap_or(end);
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
    let parent = stored
        .trace
        .as_deref()
        .and_then(crate::telemetry::SpanContext::parse);
    crate::observe::record_events(
        observability,
        &branches,
        &harness_of,
        &entries,
        parent.as_ref(),
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
fn admitted_principal_allowed(app: &App, stored: &StoredOperation) -> Result<(), ApiError> {
    let Some(principal) = &stored.principal else {
        return Ok(());
    };
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
    let policy = app.config.tenant_policy(&principal.tenant);
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

/// The branches an operation ran, once every branch they delegated to on
/// this server has finished, as `by run` waits for them.
pub(crate) fn finished(branches: Vec<Branch>) -> Result<OperationResult, branchyard::Error> {
    let mut descendants = Vec::new();
    for branch in &branches {
        descendants.extend(branch.wait_subtree()?);
    }
    Ok(OperationResult {
        branches: branches.iter().map(|b| b.info().clone()).collect(),
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
    if request.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt is empty"));
    }
    if request.harness.is_some() && !request.harnesses.is_empty() {
        return Err(ApiError::bad_request(
            "give harness for one branch or harnesses for several, not both",
        ));
    }
    let budget = request.budget.to_budget().map_err(ApiError::bad_request)?;
    let provider = app.provider(request.provider.clone())?;
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
        Some(p) => format!("{} through the server", p.name),
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
    if request.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt is empty"));
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

/// Refuse a send that keeps a delegation envelope this server does not
/// offer. Blocks: it reads the branch's envelope.
pub(crate) fn send_allowed(
    app: &App,
    target: &Branch,
    branch: &str,
    request: &SendRequest,
) -> Result<(), ApiError> {
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
        ..Spawn::default()
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

    #[test]
    fn descriptions_round_trip_and_name_their_kind() {
        let works = [
            Work::Task {
                request: TaskRequest {
                    prompt: "p".into(),
                    ..TaskRequest::default()
                },
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
            },
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
