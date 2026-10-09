//! Structured errors: an HTTP status, a stable `code`, a message for
//! people, and optional machine-readable detail.
//!
//! Codes are part of the API. SDK errors keep the SDK's own message, so a
//! remote `by` prints what a local one would.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use branchyard_client::api::{ErrorBody, ErrorResponse};
use serde_json::{json, Value};

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub body: Box<ErrorBody>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &str, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            body: Box::new(ErrorBody {
                code: code.to_owned(),
                message: message.into(),
                detail: None,
            }),
        }
    }

    pub fn detail(mut self, detail: Value) -> ApiError {
        self.body.detail = Some(detail);
        self
    }

    pub fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    pub fn internal(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    pub fn unauthorized() -> ApiError {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "a valid bearer token is required",
        )
    }

    pub fn shutting_down() -> ApiError {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "the server is shutting down and accepts no new operations",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let unauthorized = self.status == StatusCode::UNAUTHORIZED;
        let mut response = (self.status, Json(ErrorResponse { error: *self.body })).into_response();
        if unauthorized {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"branchyard\""),
            );
        }
        response
    }
}

/// The API error for an SDK error.
pub fn sdk(error: &branchyard::Error) -> ApiError {
    use branchyard::Error as E;
    use StatusCode as S;
    let message = error.to_string();
    let (status, code) = match error {
        E::NotARepository(_) => (S::INTERNAL_SERVER_ERROR, "not_a_repository"),
        E::UnknownBranch(_) => (S::NOT_FOUND, "unknown_branch"),
        E::UnknownMessage(_) => (S::NOT_FOUND, "unknown_message"),
        E::UnknownKnowledge(_) => (S::NOT_FOUND, "unknown_knowledge"),
        E::NoPlan(_) => (S::CONFLICT, "no_plan"),
        E::BranchExists(_) => (S::CONFLICT, "branch_exists"),
        E::InvalidName { .. } => (S::BAD_REQUEST, "invalid_name"),
        E::UnknownHarness(_) => (S::BAD_REQUEST, "unknown_harness"),
        E::HarnessUnavailable { .. } => (S::UNPROCESSABLE_ENTITY, "harness_unavailable"),
        E::Unsupported(_) => (S::UNPROCESSABLE_ENTITY, "unsupported"),
        E::NoCandidate(_) => (S::CONFLICT, "no_candidate"),
        E::TargetMoved { .. } => (S::CONFLICT, "target_moved"),
        E::Conflict { .. } | E::ConflictBetween { .. } => (S::CONFLICT, "conflict"),
        E::CheckFailed { .. } => (S::UNPROCESSABLE_ENTITY, "check_failed"),
        E::CheckTimedOut { .. } => (S::UNPROCESSABLE_ENTITY, "check_timed_out"),
        E::CheckNotStarted { .. } => (S::UNPROCESSABLE_ENTITY, "check_not_started"),
        E::DirtyTarget(_) => (S::CONFLICT, "dirty_target"),
        E::AlreadyMerged { .. } => (S::CONFLICT, "already_merged"),
        E::InvalidCandidate(_) => (S::UNPROCESSABLE_ENTITY, "invalid_candidate"),
        E::Denied(_) => (S::FORBIDDEN, "denied"),
        E::Running(_) => (S::CONFLICT, "running"),
        E::NotRunning(_) => (S::CONFLICT, "not_running"),
        E::Fenced(_) => (S::CONFLICT, "fenced"),
        E::StaleRevision { .. } => (S::CONFLICT, "stale_revision"),
        E::Remote { .. } => (S::BAD_GATEWAY, "remote_error"),
        E::Git(_) => (S::INTERNAL_SERVER_ERROR, "git_error"),
        E::Harness(_) => (S::INTERNAL_SERVER_ERROR, "harness_error"),
        E::Io(_) => (S::INTERNAL_SERVER_ERROR, "io_error"),
        E::State(_) => (S::INTERNAL_SERVER_ERROR, "state_error"),
    };
    let error_out = ApiError::new(status, code, message);
    match error {
        E::TargetMoved { expected, actual } => {
            error_out.detail(json!({ "expected": expected, "actual": actual }))
        }
        E::Conflict { files } => error_out.detail(json!({ "files": files })),
        E::ConflictBetween {
            branch,
            merged,
            files,
            ..
        } => error_out.detail(json!({ "files": files, "branch": branch, "merged": merged })),
        E::StaleRevision {
            expected, actual, ..
        } => error_out.detail(json!({ "expected": expected, "actual": actual })),
        E::Remote { kind, .. } => error_out.detail(json!({ "kind": kind })),
        E::CheckFailed {
            output_tail,
            checks,
            shared,
            own_work,
        } => {
            let mut detail = json!({ "output_tail": output_tail });
            if !checks.is_empty() {
                detail["checks"] = json!(checks);
            }
            if !own_work.is_empty() {
                detail["own_work"] = json!(own_work);
            }
            if let Some(shared) = shared.as_ref().and_then(|s| serde_json::to_value(s).ok()) {
                detail["shared"] = shared;
            }
            error_out.detail(detail)
        }
        E::CheckTimedOut {
            timeout,
            output_tail,
            checks,
        } => {
            let mut detail = json!({
                "timeout_seconds": timeout.as_secs_f64(),
                "output_tail": output_tail,
            });
            if !checks.is_empty() {
                detail["checks"] = json!(checks);
            }
            error_out.detail(detail)
        }
        E::CheckNotStarted { checks, .. } if !checks.is_empty() => {
            error_out.detail(json!({ "checks": checks }))
        }
        _ => error_out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_errors_keep_their_message_and_get_a_code() {
        let e = sdk(&branchyard::Error::UnknownBranch("x".into()));
        assert_eq!(
            (e.status, e.body.code.as_str(), e.body.message.as_str()),
            (StatusCode::NOT_FOUND, "unknown_branch", "no branch named x")
        );
        let e = sdk(&branchyard::Error::Conflict {
            files: vec!["a.txt".into()],
        });
        assert_eq!(e.body.detail, Some(json!({ "files": ["a.txt"] })));
    }

    /// A failed integration check names every check, its branches and its
    /// outcome in the detail, beside the failed one's output.
    #[test]
    fn a_failed_check_names_every_check_in_its_detail() {
        let e = sdk(&branchyard::Error::CheckFailed {
            output_tail: "boom".into(),
            checks: vec![
                branchyard::IntegrationCheck {
                    check: vec!["make".into(), "lint".into()],
                    branches: vec!["a".into()],
                    outcome: branchyard::CheckVerdict::Passed,
                },
                branchyard::IntegrationCheck {
                    check: vec!["make".into(), "test".into()],
                    branches: vec!["b".into(), "c".into()],
                    outcome: branchyard::CheckVerdict::Failed,
                },
            ],
            shared: None,
            own_work: Vec::new(),
        });
        assert_eq!(
            (e.status, e.body.code.as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, "check_failed")
        );
        let detail = e.body.detail.unwrap();
        assert_eq!(detail["output_tail"], "boom");
        assert_eq!(
            detail["checks"][1],
            json!({ "check": ["make", "test"], "branches": ["b", "c"], "outcome": "failed" })
        );
        assert_eq!(detail["checks"][0]["outcome"], "passed");
        assert!(e.body.message.contains("`make test` (b, c) failed"));
    }

    /// A check of an integration that could not start names every check
    /// in its detail, as a failed one does; one of a single branch's merge
    /// has none.
    #[test]
    fn a_check_that_could_not_start_names_every_check_in_its_detail() {
        let e = sdk(&branchyard::Error::CheckNotStarted {
            reason: "no such program".into(),
            checks: vec![
                branchyard::IntegrationCheck {
                    check: vec!["nope".into()],
                    branches: vec!["a".into()],
                    outcome: branchyard::CheckVerdict::NotStarted,
                },
                branchyard::IntegrationCheck {
                    check: vec!["true".into()],
                    branches: vec!["b".into()],
                    outcome: branchyard::CheckVerdict::NotRun,
                },
            ],
        });
        assert_eq!(
            (e.status, e.body.code.as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, "check_not_started")
        );
        let detail = e.body.detail.unwrap();
        assert_eq!(
            detail["checks"][0],
            json!({ "check": ["nope"], "branches": ["a"], "outcome": "not_started" })
        );
        assert_eq!(detail["checks"][1]["outcome"], "not_run");
        assert!(e.body.message.contains("`nope` (a) could not start"));
        let plain = sdk(&branchyard::Error::CheckNotStarted {
            reason: "no such program".into(),
            checks: Vec::new(),
        });
        assert_eq!(plain.body.detail, None);
    }
}
