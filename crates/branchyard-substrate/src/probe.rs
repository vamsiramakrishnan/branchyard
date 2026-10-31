//! Read-only `Control` probes that say which stage failed: no answer, a
//! refused credential, or an answer this client cannot use.

use std::fmt;

use branchyard_sandbox::ProviderError;
use serde::Serialize;
use tonic::{Code, Status};

use crate::actors::{Error, INSPECTION_TIMEOUT};
use crate::setup::{TemplateFailure, SIGNER_MISMATCH};
use crate::{pb, SubstrateProvider, TemplateInspection};

/// Why a read-only Control call failed, by who answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlError {
    /// No answer from Control: connection refused, DNS, TLS handshake, or
    /// deadline (gRPC `Unavailable`, `DeadlineExceeded`, `Cancelled`, or a
    /// status whose source is a transport error).
    Unreachable(String),
    /// Control refused the operator's credentials or their permission
    /// (`Unauthenticated`, `PermissionDenied`).
    Rejected(String),
    /// Control answered with another error (`NotFound`, `Unimplemented`,
    /// `Internal`, ...), or with a template this client cannot use.
    Answered(String),
    /// The template's bridge verifying key is not the configured signing key's.
    SignerMismatch(String),
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControlError::Unreachable(message)
            | ControlError::Rejected(message)
            | ControlError::Answered(message)
            | ControlError::SignerMismatch(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ControlError {}

/// What one ListWorkers page reported about schedulable capacity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkerCapacity {
    /// Workers on the page, in any state.
    pub listed: usize,
    /// Workers in the ACTIVE (schedulable) state.
    pub active: usize,
    /// Active workers that report an actor capacity (`status.capacity.actors > 0`).
    pub reporting: usize,
    /// Free actor slots summed over the reporting workers: capacity.actors
    /// minus allocated.actors, never below zero per worker.
    pub free_actor_slots: u64,
    /// The server has more pages that were not read (non-empty next_page_token).
    pub more: bool,
}

const WORKER_PAGE_SIZE: i32 = 1000;

impl SubstrateProvider {
    /// Read and validate the configured template through authenticated
    /// Control, saying which stage failed. Read-only: one GetActorTemplate
    /// call; creates nothing.
    pub fn probe_template(&self) -> Result<TemplateInspection, ControlError> {
        self.read_template().map_err(|failure| match failure {
            TemplateFailure::Control(error) => classify(error),
            TemplateFailure::Unusable(ProviderError::Invalid(reason)) => {
                ControlError::Answered(reason)
            }
            TemplateFailure::Unusable(error) => ControlError::Answered(error.to_string()),
            TemplateFailure::SignerMismatch => ControlError::SignerMismatch(SIGNER_MISMATCH.into()),
        })
    }

    /// List workers through authenticated Control: one ListWorkers page
    /// (page_size 1000) with the same 30-second bound as template inspection.
    /// Read-only: creates, drains and changes nothing.
    pub fn worker_capacity(&self) -> Result<WorkerCapacity, ControlError> {
        let page = self
            .run(async |actors| {
                let mut request = tonic::Request::new(pb::ListWorkersRequest {
                    page_size: WORKER_PAGE_SIZE,
                    page_token: String::new(),
                });
                request.set_timeout(INSPECTION_TIMEOUT);
                let response =
                    tokio::time::timeout(INSPECTION_TIMEOUT, actors.client.list_workers(request))
                        .await
                        .map_err(|_| {
                            Status::deadline_exceeded(
                                "Substrate worker listing exceeded 30 seconds",
                            )
                        })??;
                Ok(response.into_inner())
            })
            .map_err(classify)?;
        Ok(capacity(&page))
    }
}

fn capacity(page: &pb::ListWorkersResponse) -> WorkerCapacity {
    let mut capacity = WorkerCapacity {
        listed: page.workers.len(),
        active: 0,
        reporting: 0,
        free_actor_slots: 0,
        more: !page.next_page_token.is_empty(),
    };
    let actors = |resources: Option<&pb::WorkerResources>| resources.map_or(0, |r| r.actors);
    for status in page
        .workers
        .iter()
        .filter_map(|worker| worker.status.as_ref())
        .filter(|status| status.state() == pb::WorkerState::Active)
    {
        capacity.active += 1;
        let slots = actors(status.capacity.as_ref());
        if slots > 0 {
            capacity.reporting += 1;
            let used = actors(status.allocated.as_ref());
            capacity.free_actor_slots += u64::try_from(slots.saturating_sub(used)).unwrap_or(0);
        }
    }
    capacity
}

fn classify(error: Error) -> ControlError {
    let Error::Rpc(status) = &error else {
        return ControlError::Answered(error.to_string());
    };
    // Status's own Display carries Rust source chains; a person reads this.
    let answered = format!("{}: {}", status.code().description(), status.message());
    let unreachable = match io_kind(status) {
        Some(kind) => format!("{} ({kind})", status.message()),
        None => status.message().to_owned(),
    };
    match status.code() {
        Code::Unauthenticated | Code::PermissionDenied => ControlError::Rejected(answered),
        Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled => {
            ControlError::Unreachable(unreachable)
        }
        Code::Unknown if from_transport(status) => ControlError::Unreachable(unreachable),
        _ => ControlError::Answered(answered),
    }
}

/// tonic reports a transport failure it cannot name as `Unknown`, keeping
/// the transport error as the status's source.
fn from_transport(status: &Status) -> bool {
    let mut source = std::error::Error::source(status);
    while let Some(error) = source {
        if error.is::<tonic::transport::Error>() || error.is::<std::io::Error>() {
            return true;
        }
        source = error.source();
    }
    false
}

/// The kind of the I/O error at the root of a transport failure, such as
/// `connection refused`.
fn io_kind(status: &Status) -> Option<std::io::ErrorKind> {
    let mut source = std::error::Error::source(status);
    let mut kind = None;
    while let Some(error) = source {
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            kind = Some(io.kind());
        }
        source = error.source();
    }
    kind
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(state: pb::WorkerState, capacity: i32, allocated: i32) -> pb::Worker {
        let resources = |actors| {
            Some(pb::WorkerResources {
                actors,
                ..Default::default()
            })
        };
        pb::Worker {
            status: Some(pb::WorkerStatus {
                state: state as i32,
                capacity: resources(capacity),
                allocated: resources(allocated),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn capacity_clamps_overallocated_workers_and_reports_more_pages() {
        let page = pb::ListWorkersResponse {
            workers: vec![
                worker(pb::WorkerState::Active, 2, 5),
                worker(pb::WorkerState::Active, 3, 1),
                worker(pb::WorkerState::Unspecified, 8, 0),
                pb::Worker::default(),
            ],
            next_page_token: "next".into(),
        };
        assert_eq!(
            capacity(&page),
            WorkerCapacity {
                listed: 4,
                active: 2,
                reporting: 2,
                free_actor_slots: 2,
                more: true,
            }
        );
    }

    #[test]
    fn an_unknown_status_is_unreachable_only_with_a_transport_source() {
        let transport = Status::from_error(Box::new(std::io::Error::from(
            std::io::ErrorKind::ConnectionReset,
        )));
        assert_eq!(transport.code(), Code::Unknown);
        match classify(Error::Rpc(transport)) {
            ControlError::Unreachable(said) => {
                assert!(said.ends_with("(connection reset)"), "{said}");
                assert!(!said.contains("source"), "no Rust source chain: {said}");
            }
            other => panic!("not unreachable: {other:?}"),
        }
        assert_eq!(
            classify(Error::Rpc(Status::unauthenticated("token expired"))),
            ControlError::Rejected(
                "The request does not have valid authentication credentials: token expired".into()
            )
        );
        assert!(matches!(
            classify(Error::Rpc(Status::unknown("server fault"))),
            ControlError::Answered(_)
        ));
        assert!(matches!(
            classify(Error::MissingField("worker.status")),
            ControlError::Answered(_)
        ));
    }
}
