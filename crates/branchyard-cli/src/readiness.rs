//! The seven readiness checks for a `[remote]` server, from what it
//! answered: `by setup --up` reads its worker inventory, `by doctor` its
//! repository list. The engine computes them for this machine and for
//! Substrate ([`branchyard::up::readiness_checks`]); a server's are here
//! because only the CLI speaks to one. Classification reads typed errors
//! (an HTTP status, a transport failure, a report's kind), never prose.

use branchyard::up::{HarnessFindings, ReadinessCheck, ReadinessCheckId};
use branchyard_client::api::InventoryReport;

use crate::commands::Failure;

/// What a configured server answered, or why nothing was asked.
#[derive(Debug)]
pub(crate) enum Answer {
    /// Not asked here: an `ssh://` remote, which `by doctor` leaves alone.
    NotAsked(String),
    /// The request could not be made: no token file, an unreadable token or
    /// CA file, or a missing `ssh`. Nothing was sent.
    Unusable(String),
    /// No answer: connection, TLS or timeout.
    Unreachable(String),
    /// It answered and refused the token (401 or 403).
    Refused(String),
    /// It answered with another error, or with something this client does
    /// not understand.
    Failed(String),
    /// Its repository list, and the repository `[remote] repo` names.
    Repositories {
        /// The repositories it serves.
        served: Vec<String>,
        /// The one the configuration wants, if it names one.
        wanted: Option<String>,
    },
    /// Its live workers and what each advertised, for `repository`.
    Workers {
        /// `GET /v1/inventory`.
        report: InventoryReport,
        /// The repository the configuration acts on.
        repository: String,
    },
}

impl Answer {
    /// A client error, by its kind and HTTP status.
    pub(crate) fn of_client(error: &branchyard_client::Error) -> Answer {
        use branchyard_client::Error;
        match error {
            Error::Api {
                status: 401 | 403, ..
            } => Answer::Refused(error.to_string()),
            Error::Api { .. } | Error::Protocol(_) => Answer::Failed(error.to_string()),
            Error::Transport { .. } => Answer::Unreachable(error.to_string()),
            Error::Config(_) => Answer::Unusable(error.to_string()),
        }
    }

    /// A failure connecting (`Remote::connect`), by its client error or its
    /// report's kind.
    pub(crate) fn of_failure(failure: &Failure) -> Answer {
        match failure {
            Failure::Remote(error) => Answer::of_client(error),
            Failure::Report(report) => match report.kind.as_str() {
                "remote_unreachable" => Answer::Unreachable(report.said()),
                "remote_protocol" => Answer::Failed(report.said()),
                _ => Answer::Unusable(report.said()),
            },
            other => Answer::Unusable(other.to_string()),
        }
    }
}

/// The seven checks for the server at `shown` (its URL without any
/// `user:password@`), from `answer`, for `harness`.
pub(crate) fn server_checks(shown: &str, answer: &Answer, harness: &str) -> Vec<ReadinessCheck> {
    use ReadinessCheckId::*;
    let not_read = |id| {
        ReadinessCheck::unknown(id, "not checked: the server's workers were not read")
            .next("by setup --up reads the server's worker inventory")
    };
    // The failing credential or control-plane check says what to fix first.
    let not_reached =
        |id| ReadinessCheck::unknown(id, "not checked until the server answers with its workers");
    let capabilities = ReadinessCheck::unknown(
        ExecutionCapabilitiesSupported,
        "checked when a task starts: the server refuses a provider or isolation it cannot honour",
    );
    let configuration = ReadinessCheck::verified(
        ConfigurationValid,
        format!("the configuration loaded and names the server {shown}"),
    );
    let (credentials, control) = match answer {
        Answer::NotAsked(why) => (
            ReadinessCheck::unknown(OperatorCredentialsAvailable, format!("not checked: {why}"))
                .next("by remote ssh status"),
            ReadinessCheck::unknown(RemoteControlPlaneReachable, format!("not checked: {why}"))
                .next("by remote ssh status"),
        ),
        Answer::Unusable(why) => (
            ReadinessCheck::failed(
                OperatorCredentialsAvailable,
                why,
                "set token_file (and ca_file) under [remote], or pass --token-file and --ca-file",
            ),
            ReadinessCheck::unknown(
                RemoteControlPlaneReachable,
                "not checked: no request was sent without usable credentials",
            ),
        ),
        Answer::Unreachable(why) => (
            ReadinessCheck::unverified(
                OperatorCredentialsAvailable,
                "the token file is readable here; the server did not answer, so it has not accepted it",
            ),
            ReadinessCheck::failed(
                RemoteControlPlaneReachable,
                format!("{shown} did not answer: {why}"),
                crate::remote::UNREACHABLE_FIX,
            ),
        ),
        Answer::Refused(why) => (
            ReadinessCheck::failed(
                OperatorCredentialsAvailable,
                format!("{shown} refused the token: {why}"),
                "replace the token in the file [remote] token_file names with one the server issued",
            ),
            ReadinessCheck::verified(RemoteControlPlaneReachable, format!("{shown} answered")),
        ),
        Answer::Failed(why) => (
            ReadinessCheck::unknown(
                OperatorCredentialsAvailable,
                "not established: the server's answer was an error",
            ),
            ReadinessCheck::failed(
                RemoteControlPlaneReachable,
                format!("{shown} answered with an error: {why}"),
                "check the server's log, then rerun by setup --up",
            ),
        ),
        Answer::Repositories { .. } | Answer::Workers { .. } => (
            ReadinessCheck::verified(OperatorCredentialsAvailable, "the server accepted the token"),
            ReadinessCheck::verified(RemoteControlPlaneReachable, format!("{shown} answered")),
        ),
    };
    let (installed, capacity, auth) = match answer {
        Answer::Repositories { served, wanted } => {
            let capacity = match wanted {
                Some(wanted) if !served.contains(wanted) => ReadinessCheck::failed(
                    WorkerCapacityAvailable,
                    format!("{shown} serves {}, not {wanted:?}", served.join(", ")),
                    "fix repo under [remote] (or --repo / BRANCHYARD_REPO), or serve it",
                ),
                _ => not_read(WorkerCapacityAvailable),
            };
            (
                not_read(CliAndHarnessInstalled),
                capacity,
                not_read(HarnessAuthVerified),
            )
        }
        Answer::Workers { report, repository } => workers(report, repository, harness),
        Answer::NotAsked(_) => (
            not_read(CliAndHarnessInstalled),
            not_read(WorkerCapacityAvailable),
            not_read(HarnessAuthVerified),
        ),
        _ => (
            not_reached(CliAndHarnessInstalled),
            not_reached(WorkerCapacityAvailable),
            not_reached(HarnessAuthVerified),
        ),
    };
    vec![
        installed,
        configuration,
        credentials,
        control,
        capacity,
        capabilities,
        auth,
    ]
}

/// Installed, capacity and authentication from the live workers that
/// serve `repository`, as each one's own detection reported its harness.
fn workers(
    report: &InventoryReport,
    repository: &str,
    harness: &str,
) -> (ReadinessCheck, ReadinessCheck, ReadinessCheck) {
    use ReadinessCheckId::*;
    let serving: Vec<_> = report
        .workers
        .iter()
        .filter(|worker| worker.repos.iter().any(|name| name == repository))
        .collect();
    if serving.is_empty() {
        let none = format!("no live worker serves {repository}");
        return (
            ReadinessCheck::unknown(CliAndHarnessInstalled, &none),
            ReadinessCheck::failed(
                WorkerCapacityAvailable,
                &none,
                format!("start a worker that serves {repository}, then rerun by setup --up"),
            ),
            ReadinessCheck::unknown(HarnessAuthVerified, &none),
        );
    }
    let capacity = ReadinessCheck::verified(
        WorkerCapacityAvailable,
        format!(
            "{} live worker(s) serve {repository}; tasks queue on them",
            serving.len()
        ),
    );
    let found = serving.iter().find_map(|worker| {
        let state = worker
            .inventory
            .as_ref()?
            .harnesses
            .iter()
            .find(|state| state.id == harness && state.on_path)?;
        Some((worker.id.as_str(), HarnessFindings::of(state)))
    });
    match found {
        Some((worker, found)) => {
            let on = |mut check: ReadinessCheck| {
                check.detail = format!("worker {worker} reports {}", check.detail);
                check
            };
            (on(found.installed), capacity, on(found.auth))
        }
        None => (
            ReadinessCheck::failed(
                CliAndHarnessInstalled,
                format!("no worker serving {repository} advertises {harness} on PATH"),
                format!("install {harness} on the worker, then rerun by setup --up"),
            ),
            capacity,
            ReadinessCheck::unknown(
                HarnessAuthVerified,
                format!("{harness} is not installed on a worker serving {repository}"),
            )
            .next(format!(
                "install {harness} on the worker, then rerun by setup --up"
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::inventory::{Evidence, HarnessState, Inventory, Login, LoginState};
    use branchyard::up::CheckStatus::{self, *};
    use branchyard_client::api::WorkerInventory;

    fn statuses(checks: &[ReadinessCheck]) -> Vec<CheckStatus> {
        checks.iter().map(|c| c.status).collect()
    }

    fn report(login: Login) -> InventoryReport {
        InventoryReport {
            workers: vec![WorkerInventory {
                id: "w1".into(),
                host: "host".into(),
                labels: Vec::new(),
                repos: vec!["project".into()],
                seen_ms_ago: 0,
                this: true,
                inventory: Some(Inventory {
                    harnesses: vec![HarnessState {
                        id: "antigravity".into(),
                        path: "/usr/local/bin/agy".into(),
                        on_path: true,
                        version: Some("1.4.8".into()),
                        login,
                        ..HarnessState::default()
                    }],
                    ..Inventory::default()
                }),
            }],
        }
    }

    fn workers(report: InventoryReport, repository: &str) -> Vec<ReadinessCheck> {
        let answer = Answer::Workers {
            report,
            repository: repository.into(),
        };
        server_checks("https://server.example", &answer, "antigravity")
    }

    #[test]
    fn a_serving_worker_reports_installed_capacity_and_its_own_login_evidence() {
        let likely = Login {
            state: LoginState::LoggedIn,
            evidence: Evidence::Likely,
            detail: "GEMINI_API_KEY is set".into(),
            key: Some("GEMINI_API_KEY".into()),
            ..Login::default()
        };
        let checks = workers(report(likely), "project");
        assert_eq!(
            statuses(&checks),
            [Verified, Verified, Verified, Verified, Verified, Unknown, Unverified]
        );
        assert!(checks[0]
            .detail
            .starts_with("worker w1 reports antigravity"));
        let verified = Login {
            state: LoginState::LoggedIn,
            evidence: Evidence::Verified,
            detail: "signed in".into(),
            ..Login::default()
        };
        assert_eq!(workers(report(verified), "project")[6].status, Verified);
        let out = Login {
            state: LoginState::LoggedOut,
            evidence: Evidence::Verified,
            ..Login::default()
        };
        let checks = workers(report(out), "project");
        assert_eq!(checks[6].status, Failed);
        assert!(checks[6].next.is_some());
    }

    #[test]
    fn no_serving_worker_fails_capacity_and_leaves_the_harness_unknown() {
        let checks = workers(report(Login::default()), "another");
        assert_eq!(checks[4].status, Failed);
        assert!(checks[4].next.as_deref().unwrap().contains("another"));
        assert_eq!(checks[0].status, Unknown);
        assert_eq!(checks[6].status, Unknown);
        let mut missing = report(Login::default());
        missing.workers[0].inventory = None;
        let checks = workers(missing, "project");
        assert_eq!(checks[0].status, Failed);
        assert_eq!(checks[4].status, Verified);
    }

    #[test]
    fn each_failed_answer_lands_on_credentials_or_the_control_plane() {
        let shown = "https://server.example";
        let cases = [
            (Answer::Unusable("no token file".into()), [Failed, Unknown]),
            (Answer::Unreachable("refused".into()), [Unverified, Failed]),
            (Answer::Refused("expired token".into()), [Failed, Verified]),
            (Answer::Failed("500".into()), [Unknown, Failed]),
            (Answer::NotAsked("an ssh remote".into()), [Unknown, Unknown]),
        ];
        for (answer, expected) in cases {
            let checks = server_checks(shown, &answer, "antigravity");
            assert_eq!([checks[2].status, checks[3].status], expected, "{answer:?}");
            assert_eq!(checks[0].status, Unknown, "{answer:?}");
            assert_eq!(checks[4].status, Unknown, "{answer:?}");
            if !matches!(answer, Answer::NotAsked(_)) {
                assert!(
                    checks[0].next.is_none(),
                    "the failing check leads: {answer:?}"
                );
            }
            for check in &checks {
                if check.status == Failed {
                    assert!(check.next.is_some(), "{answer:?}: {check:?}");
                }
            }
        }
    }

    #[test]
    fn client_errors_and_reports_classify_by_status_and_kind() {
        use branchyard_client::Error;
        let transport = Error::Transport {
            endpoint: "https://server.example".into(),
            message: "connection refused".into(),
        };
        assert!(matches!(
            Answer::of_client(&transport),
            Answer::Unreachable(_)
        ));
        assert!(matches!(
            Answer::of_client(&Error::Config("cannot read the token file".into())),
            Answer::Unusable(_)
        ));
        assert!(matches!(
            Answer::of_client(&Error::Protocol("not JSON".into())),
            Answer::Failed(_)
        ));
        let refused = |status| Error::Api {
            status,
            error: Box::new(branchyard_client::api::ErrorBody {
                code: "unauthorized".into(),
                message: "the token has expired".into(),
                detail: None,
            }),
        };
        assert!(matches!(
            Answer::of_client(&refused(401)),
            Answer::Refused(_)
        ));
        assert!(matches!(
            Answer::of_client(&refused(403)),
            Answer::Refused(_)
        ));
        assert!(matches!(
            Answer::of_client(&refused(500)),
            Answer::Failed(_)
        ));
        let unreachable: Failure =
            crate::errors::Report::new("remote_unreachable", "ssh could not connect").into();
        assert!(matches!(
            Answer::of_failure(&unreachable),
            Answer::Unreachable(_)
        ));
        let usage: Failure = crate::errors::Report::usage("no token file").into();
        assert!(matches!(Answer::of_failure(&usage), Answer::Unusable(_)));
    }

    #[test]
    fn a_repository_list_checks_the_wanted_repository_and_reads_no_workers() {
        let answer = Answer::Repositories {
            served: vec!["one".into()],
            wanted: Some("two".into()),
        };
        let checks = server_checks("https://server.example", &answer, "antigravity");
        assert_eq!(checks[4].status, Failed);
        assert_eq!(checks[0].status, Unknown);
        assert!(checks[0].next.as_deref().unwrap().contains("by setup --up"));
    }
}
