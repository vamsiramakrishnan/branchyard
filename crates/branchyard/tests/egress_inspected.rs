//! A rule naming a method and a path, held to inside the connection, in a
//! test binary of its own: the engine trusts the origin's authority through
//! its own `SSL_CERT_FILE`, which it reads in this process, so the test sets
//! it for the whole process. Beside other tests that would change what they
//! trust; alone it changes nothing else. Hermetic: every host is a listener
//! on this machine's loopback.

#![allow(clippy::panic, clippy::unwrap_in_result, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;
mod egress_common;

use std::fs;
use std::time::Duration;

use branchyard::{
    BranchStatus, Budget, Network, NetworkEnforce, Policy, Provisioning, TaskOptions,
};
use common::{text, Fixture};
use egress_common::{bundle, probe, prompt, requests, tls_upstream, upstream};

/// Without confinement, as on a host without user namespaces.
fn fixture() -> Fixture {
    std::env::set_var(branchyard_runtime::ENV_EGRESS_NETNS, "off");
    Fixture::new()
}

fn network(rules: &[String], enforce: NetworkEnforce) -> Option<Provisioning> {
    Some(Provisioning {
        network: Some(Network::from_rules(rules, enforce).unwrap()),
        ..Provisioning::default()
    })
}

fn options(f: &Fixture, provision: Option<Provisioning>) -> TaskOptions {
    TaskOptions {
        policy: Policy::allow_all(),
        provision,
        budget: Budget {
            max_duration: Some(Duration::from_secs(120)),
            ..Budget::default()
        },
        ..f.options()
    }
}

/// A rule naming a method and a path is held to inside the connection:
/// the proxy terminates TLS with the branch's own authority, which the
/// harness trusts through the bundle the trust variables name; a plain
/// request is held to the same rule; the authority lives with the branch.
#[test]
fn an_inspected_rule_terminates_tls_and_holds_each_request_to_it() {
    use std::os::unix::fs::PermissionsExt as _;
    let f = fixture();
    let (origin, origin_pem) = tls_upstream();
    let plain = upstream();
    // The engine trusts the origin as it would a corporate authority:
    // through its own SSL_CERT_FILE, which also heads the harness's bundle.
    let roots = f.dir.join("origin-roots.pem");
    fs::write(&roots, &origin_pem).unwrap();
    std::env::set_var("SSL_CERT_FILE", &roots);
    let probe = probe(&f.dir);
    let rules = [
        format!("GET 127.0.0.1:{}/x", origin.port),
        format!("GET 127.0.0.1:{}/x", plain.port),
    ];
    let mut text_of = prompt(
        &probe,
        &[
            ("https", origin.port),
            ("https-post", origin.port),
            ("proxy", plain.port),
        ],
    );
    text_of.push_str(
        "\nSH echo trust=$SSL_CERT_FILE,$NODE_EXTRA_CA_CERTS,$REQUESTS_CA_BUNDLE,$GIT_SSL_CAINFO",
    );
    let options = options(&f, network(&rules, NetworkEnforce::BestEffort));
    let branch = f
        .task(&text_of)
        .options(options.clone())
        .name("inspected")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    assert_eq!(branch.info().status, BranchStatus::NoChanges, "{events:?}");
    let said = text(&events);
    for line in [
        format!("https {0}: upstream {0}", origin.port),
        format!("https-post {}: 403", origin.port),
        format!("proxy {0}: upstream {0}", plain.port),
    ] {
        assert!(said.contains(&line), "{line} not in {said}");
    }
    // The authority and its bundle, under the branch's egress directory.
    let dir = f.root.join(".branchyard/egress/inspected");
    let bundle_path = dir.join("ca-bundle.pem");
    assert_eq!(
        bundle(&events).as_deref(),
        Some(bundle_path.to_str().unwrap())
    );
    let ca = fs::read_to_string(dir.join("ca.pem")).unwrap();
    let bundled = fs::read_to_string(&bundle_path).unwrap();
    assert!(
        bundled.starts_with(&origin_pem),
        "the system roots come first"
    );
    assert!(bundled.ends_with(&ca), "then the authority");
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(dir.join("ca.key"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(
        said.contains(&format!("trust={0},{0},{0},{0}", bundle_path.display())),
        "{said}"
    );
    // Each request decided by method and path, the TLS ones inside.
    let inspected = Some("inspected".to_owned());
    assert_eq!(
        requests(&events),
        [
            ("CONNECT".to_owned(), None, true, inspected.clone()),
            ("GET".to_owned(), Some("/x".to_owned()), true, None),
            ("CONNECT".to_owned(), None, true, inspected),
            (
                "POST".to_owned(),
                Some("/x".to_owned()),
                false,
                Some("no rule allows it".to_owned())
            ),
            ("GET".to_owned(), Some("/x".to_owned()), true, None),
        ]
    );
    assert_eq!(origin.requests.try_recv().unwrap(), "GET /x HTTP/1.1");
    assert!(
        origin.requests.try_recv().is_err(),
        "the POST reached the origin"
    );
    assert_eq!(plain.requests.try_recv().unwrap(), "GET /x HTTP/1.1");
    // The authority is the branch's: a second turn keeps it.
    branch.send("say hi", options).unwrap();
    assert_eq!(fs::read_to_string(dir.join("ca.pem")).unwrap(), ca);
    // And goes with the branch.
    f.yard.remove("inspected").unwrap();
    assert!(!dir.exists(), "the authority outlived its branch");
}
