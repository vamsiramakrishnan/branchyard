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
    ErrorBody, ForkRequest, OperationKind, OperationResult, ReincarnateRequest, SendRequest,
    SpawnRequest, TaskRequest,
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
        }
    }

    /// The queue row's JSON.
    pub fn to_value(&self) -> Result<Value, ApiError> {
        serde_json::to_value(self)
            .map_err(|e| ApiError::internal(format!("could not describe the operation: {e}")))
    }

    /// Run the work against `repo` on `app`'s configuration.
    fn run(self, app: &App, repo: &RepoState) -> Result<OperationResult, ErrorBody> {
        let sdk = |e: branchyard::Error| *error::sdk(&e).body;
        let api = |e: ApiError| *e.body;
        let yard = &repo.yard;
        match self {
            Work::Task { request } => {
                let options = task_options(app, repo, &request).map_err(api)?;
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
                let options = send_options(app, repo, &request).map_err(api)?;
                let target = yard.branch(&branch).map_err(sdk)?;
                send_allowed(app, &target, &branch, &request).map_err(api)?;
                target
                    .send(&request.prompt, options)
                    .and_then(|b| finished(vec![b]))
                    .map_err(sdk)
            }
            Work::Fork { branch, request } => {
                let options = fork_options(app, repo, &request).map_err(api)?;
                yard.branch(&branch)
                    .and_then(|source| source.fork(&request.prompt, request.fresh_session, options))
                    .and_then(|b| finished(vec![b]))
                    .map_err(sdk)
            }
            Work::Reincarnate { branch, request } => {
                let options = reincarnate_options(app, repo, &request).map_err(api)?;
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
                let (options, spawn) = spawn_parts(app, repo, &request, name).map_err(api)?;
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
        let result = serde_json::from_value::<Work>(work.clone())
            .map_err(|e| *ApiError::internal(format!("unreadable operation description: {e}")).body)
            .and_then(|work| work.run(app, repo));
        // Read the feed's head so the end cursor covers all the activity.
        let end_cursor = match repo.feed.sync() {
            Ok(head) => Some(head),
            Err(e) => {
                eprintln!("branchyard-server: could not read the event feed: {e}");
                None
            }
        };
        Finished { result, end_cursor }
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
}
