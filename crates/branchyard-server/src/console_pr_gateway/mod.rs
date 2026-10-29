//! Workspace PR routes with injected live authorization and durable admission.
//!
//! The embedding worker owns claims, fencing and the existing branch lock.
//! This adapter never starts forge work from an HTTP request or owns a queue.

mod routes;

pub use routes::router;

use std::fmt;
use std::sync::Arc;

use axum::http::StatusCode;
use branchyard_client::api::Operation;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::Caller;
use crate::console_pr::{AuthorizedBranch, Error, Publish, Service};
use crate::error::ApiError;

const MAX_RESPONSE: usize = 2 * 1024 * 1024;

/// Scope and membership decisions required from the current console resolver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrAccess {
    /// Read scope and workspace membership.
    Inspect,
    /// Run scope and workspace editor membership for queued observation refresh.
    Refresh,
    /// Merge scope, workspace editor membership and repository publication policy.
    Publish,
}

impl PrAccess {
    /// The ordinary server scope intersected with the resolver's console checks.
    fn scope(self) -> &'static str {
        match self {
            Self::Inspect => "read",
            Self::Refresh => "run",
            Self::Publish => "merge",
        }
    }
}

/// A candidate-bound command stored inside the existing durable console work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum PrCommand {
    /// Read fresh forge/check/review observations without publishing a candidate.
    Refresh,
    /// Publish the exact candidate observed during admission.
    Publish {
        /// Native session branch captured during admission, never an HTTP body field.
        expected_branch: String,
        /// Engine commit identity captured by the gateway, never an HTTP body field.
        expected_candidate: String,
        /// Validated plain-text publication choices.
        request: Publish,
    },
}

impl PrCommand {
    /// Canonical client intent for queue idempotency, excluding the server-derived
    /// candidate. Bind this to caller, workspace and session in the queue key.
    pub fn request_identity(&self) -> Value {
        match self {
            Self::Refresh => json!({"action":"refresh"}),
            Self::Publish { request, .. } => json!({"action":"publish","request":request}),
        }
    }

    /// Recheck the same authority when the claimed worker eventually executes.
    fn access(&self) -> PrAccess {
        match self {
            Self::Refresh => PrAccess::Refresh,
            Self::Publish { .. } => PrAccess::Publish,
        }
    }
}

/// Re-read caller revocation, tenant/repository policy, membership and session
/// binding. Returned contexts must also use a live service authorizer callback.
pub type Resolver =
    dyn Fn(&Caller, &str, &str, PrAccess) -> Result<AuthorizedBranch, ApiError> + Send + Sync;

/// Atomically reauthorize and admit command plus request key into the existing
/// operation store. Repeated keys return its existing operation, including an
/// interrupted outcome; differing request identities conflict. The server-derived
/// candidate is persisted with work but excluded from idempotency identity.
/// No forge work happens here.
pub type Queue =
    dyn Fn(&Caller, &str, &str, PrCommand, &str) -> Result<Operation, ApiError> + Send + Sync;

/// Claimed worker output attributed to the stable durable operation identity.
#[derive(Clone, Debug, Serialize)]
pub struct PrCompleted {
    /// Existing queue operation ID, never allocated by this adapter.
    pub operation_id: String,
    /// Publication result or refreshed durable/forge observations.
    pub result: Value,
}

/// Claimed work failure retaining its operation and effect uncertainty.
#[derive(Clone, Debug)]
pub struct PrExecutionError {
    /// Existing operation whose state must be reconciled.
    pub operation_id: String,
    /// Mark unknown effects interrupted/uncertain; never automatically replay.
    pub error: Error,
}

impl fmt::Display for PrExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for PrExecutionError {}

/// PR observation and durable admission independent of central application state.
pub struct PrGateway {
    service: Service,
    resolve: Arc<Resolver>,
    queue: Option<Arc<Queue>>,
}

impl PrGateway {
    /// Use fixed operator executables and console-owned callbacks.
    pub fn new(service: Service, resolve: Arc<Resolver>, queue: Option<Arc<Queue>>) -> Self {
        Self {
            service,
            resolve,
            queue,
        }
    }

    /// Intersect ordinary scopes with a fresh console resolution.
    fn authorize(
        &self,
        caller: &Caller,
        workspace: &str,
        session: &str,
        access: PrAccess,
    ) -> Result<AuthorizedBranch, ApiError> {
        if !caller.0.allows(access.scope()) {
            return Err(crate::api::scope_required(access.scope()));
        }
        (self.resolve)(caller, workspace, session, access)
    }

    /// Withhold results if the grant or managed session binding changed mid-call.
    fn recheck(
        &self,
        caller: &Caller,
        workspace: &str,
        session: &str,
        access: PrAccess,
        original: &AuthorizedBranch,
    ) -> Result<(), ApiError> {
        let current = self.authorize(caller, workspace, session, access)?;
        if !original.same_binding(&current) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "workspace_binding_changed",
                "the workspace session binding changed",
            ));
        }
        Ok(())
    }

    /// Observe engine state without contacting the forge.
    fn inspect(&self, caller: &Caller, workspace: &str, session: &str) -> Result<Value, ApiError> {
        let target = self.authorize(caller, workspace, session, PrAccess::Inspect)?;
        let snapshot = self.service.inspect(&target).map_err(api_error)?;
        self.recheck(caller, workspace, session, PrAccess::Inspect, &target)?;
        let value = serde_json::to_value(snapshot)
            .map_err(|error| ApiError::internal(error.to_string()))?;
        bounded(value, false).map_err(api_error)
    }

    /// Capture a publication candidate and hand its checked command to durable admission.
    fn admit(
        &self,
        caller: &Caller,
        workspace: &str,
        session: &str,
        request: Option<Publish>,
        key: &str,
    ) -> Result<Operation, ApiError> {
        let access = if request.is_some() {
            PrAccess::Publish
        } else {
            PrAccess::Refresh
        };
        let target = self.authorize(caller, workspace, session, access)?;
        let command = match request {
            Some(request) => {
                request.validate().map_err(api_error)?;
                let snapshot = self.service.inspect(&target).map_err(api_error)?;
                let expected_candidate = snapshot.candidate.ok_or_else(|| {
                    ApiError::new(
                        StatusCode::CONFLICT,
                        "no_candidate",
                        "this conversation has no code candidate to publish",
                    )
                })?;
                PrCommand::Publish {
                    expected_branch: snapshot.branch,
                    expected_candidate,
                    request,
                }
            }
            None => PrCommand::Refresh,
        };
        let queue = self.queue.as_ref().ok_or_else(|| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "pr_queue_unavailable",
                "the server has no durable PR queue configured",
            )
        })?;
        let operation = queue(caller, workspace, session, command, key)?;
        self.recheck(caller, workspace, session, access, &target)?;
        Ok(operation)
    }

    /// Execute only under the caller's existing durable claim and branch lock.
    /// Recheck current credentials and the admitted candidate before effects.
    /// Unknown-effect errors must become interrupted/uncertain queue outcomes.
    pub async fn execute(
        &self,
        caller: &Caller,
        workspace: &str,
        session: &str,
        command: &PrCommand,
        operation_id: &str,
    ) -> Result<PrCompleted, PrExecutionError> {
        let result = self
            .execute_inner(caller, workspace, session, command, operation_id)
            .await;
        result.map_err(|error| PrExecutionError {
            operation_id: operation_id.into(),
            error,
        })
    }

    /// Run the fixed operator service and recheck binding before returning its result.
    async fn execute_inner(
        &self,
        caller: &Caller,
        workspace: &str,
        session: &str,
        command: &PrCommand,
        operation_id: &str,
    ) -> Result<PrCompleted, Error> {
        if !valid_key(operation_id) {
            return Err(Error::new(
                "invalid_operation",
                "the durable PR operation identity is invalid",
                false,
            ));
        }
        let access = command.access();
        let target = self
            .authorize(caller, workspace, session, access)
            .map_err(service_error)?;
        let result = match command {
            PrCommand::Refresh => serde_json::to_value(self.service.refresh(&target).await?)
                .map_err(|_| {
                    Error::new(
                        "pr_result_invalid",
                        "PR observations could not be encoded",
                        false,
                    )
                })?,
            PrCommand::Publish {
                expected_branch,
                expected_candidate,
                request,
            } => {
                let current = self.service.inspect(&target)?;
                if current.branch != *expected_branch {
                    return Err(Error::new(
                        "workspace_binding_changed",
                        "the native session binding changed after PR admission",
                        false,
                    ));
                }
                if current.candidate.as_deref() != Some(expected_candidate.as_str()) {
                    return Err(Error::new("stale_candidate", "the code candidate changed after PR admission; review it before publishing", false));
                }
                self.service.publish(&target, request).await?
            }
        };
        self.recheck(caller, workspace, session, access, &target)
            .map_err(|error| {
                let mut error = service_error(error);
                error.unknown_effect = access == PrAccess::Publish;
                error
            })?;
        let result = bounded(result, access == PrAccess::Publish)?;
        Ok(PrCompleted {
            operation_id: operation_id.into(),
            result,
        })
    }
}

/// Bound complete HTTP/worker PR values without truncating JSON or hiding effects.
fn bounded(value: Value, publication: bool) -> Result<Value, Error> {
    let bytes = serde_json::to_vec(&value).map_err(|_| {
        Error::new(
            "pr_result_invalid",
            "PR observations could not be encoded",
            publication,
        )
    })?;
    if bytes.len() > MAX_RESPONSE {
        return Err(Error::new(
            "pr_result_too_large",
            "PR observations exceed 2 MiB; inspect narrower check logs and forge views",
            publication,
        ));
    }
    Ok(value)
}

/// Adapt a console authorization error without losing its machine category.
fn service_error(error: ApiError) -> Error {
    Error::new(error.body.code.clone(), error.body.message.clone(), false)
}

/// Keep service uncertainty explicit in the ordinary HTTP error envelope.
fn api_error(error: Error) -> ApiError {
    let status = match error.code.as_str() {
        "denied" => StatusCode::FORBIDDEN,
        "no_candidate" | "candidate_changed" | "stale_candidate" => StatusCode::CONFLICT,
        "invalid_pr_request" => StatusCode::BAD_REQUEST,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    };
    ApiError::new(status, &error.code, error.message)
        .detail(json!({"unknown_effect":error.unknown_effect}))
}

/// A visible bounded key used by the existing queue, never as a shell argument.
fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 255 && key.bytes().all(|byte| byte.is_ascii_graphic())
}
