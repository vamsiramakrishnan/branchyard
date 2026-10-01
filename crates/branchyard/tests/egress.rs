//! Egress enforced: a local harness confined to its own network namespace
//! reaches what its policy allows through the proxy, and nothing directly.
//! Needs a host that allows unprivileged user and network namespaces; on
//! one that does not, the test says why and skips, and fails if
//! `unshare -rn` works there (then the fault is ours). The advisory and
//! refused cases are in `egress_advisory.rs`, which turns confinement off.

mod common;
mod egress_common;

use std::time::Duration;

use branchyard::{
    BranchStatus, Budget, EgressEnforcement, Network, NetworkEnforce, Policy, Provisioning,
    TaskOptions,
};
use common::{text, Fixture};
use egress_common::{applied, decisions, probe, prompt, upstream};

/// Why this host cannot confine a harness, when that is the kernel's
/// doing.
fn unavailable() -> Option<String> {
    let why = branchyard_runtime::LocalProvider::confinement().err()?;
    let unshare = std::process::Command::new("unshare")
        .args(["-rn", "true"])
        .status();
    if unshare.is_ok_and(|s| s.success()) {
        panic!("unshare -rn works on this host but confinement does not: {why}");
    }
    Some(why)
}

#[test]
fn a_confined_harness_reaches_only_what_its_policy_allows() {
    if let Some(why) = unavailable() {
        eprintln!("SKIPPED a_confined_harness_reaches_only_what_its_policy_allows: {why}");
        return;
    }
    let f = Fixture::new();
    let (allowed, denied) = (upstream(), upstream());
    let probe = probe(&f.dir);
    let network = Network::from_rules(
        &[format!("127.0.0.1:{}", allowed.port)],
        NetworkEnforce::Required,
    )
    .unwrap();
    let branch = f
        .task(&prompt(
            &probe,
            &[
                ("proxy", allowed.port),
                ("proxy", denied.port),
                ("direct", allowed.port),
                ("direct", denied.port),
            ],
        ))
        .options(TaskOptions {
            policy: Policy::allow_all(),
            provision: Some(Provisioning {
                network: Some(network),
                ..Provisioning::default()
            }),
            budget: Budget {
                max_duration: Some(Duration::from_secs(120)),
                ..Budget::default()
            },
            ..f.options()
        })
        .name("confined")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    assert_eq!(branch.info().status, BranchStatus::NoChanges, "{events:?}");
    let said = text(&events);
    for line in [
        format!("proxy {0}: upstream {0}", allowed.port),
        format!("proxy {}: 403", denied.port),
        format!("direct {}: blocked", allowed.port),
        format!("direct {}: blocked", denied.port),
        "vars=http://127.0.0.1:3128,http://127.0.0.1:3128,http://127.0.0.1:3128,[]".to_owned(),
    ] {
        assert!(said.contains(&line), "{line} not in {said}");
    }
    assert_eq!(applied(&events), [(EgressEnforcement::Enforced, None)]);
    assert_eq!(
        decisions(&events),
        [
            (
                "GET".to_owned(),
                allowed.port,
                true,
                Some(format!("127.0.0.1:{}", allowed.port))
            ),
            ("GET".to_owned(), denied.port, false, None),
        ]
    );
    assert_eq!(allowed.requests.try_recv().unwrap(), "GET /x HTTP/1.1");
    assert!(allowed.requests.try_recv().is_err(), "reached directly");
    assert!(
        denied.requests.try_recv().is_err(),
        "the denied host was reached"
    );
}
