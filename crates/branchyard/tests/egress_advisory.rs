//! Egress where it cannot be enforced, and what holds everywhere: with
//! confinement turned off (`BRANCHYARD_EGRESS_NETNS=off`, as on a host
//! without user namespaces), a best-effort policy runs with the proxy's
//! variables and says it is advisory, a required one is refused before
//! anything is created, the connector gateway (through the turn's
//! effect-ledger proxy) is allowed on its own, and a
//! delegated child's policy is never wider than its parent's. Hermetic:
//! every host is a listener on this machine's loopback.

mod common;
mod egress_common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use branchyard::connectors::{Bundle, Gateway, GrantEntry, Packager};
use branchyard::{
    Activity, BranchStatus, Budget, ChildBudget, EgressEnforcement, Error, Network, NetworkEnforce,
    Policy, Provisioning, Seat, Seats, Spawn, TaskOptions,
};
use common::{fake_agent, stored_record, text, Fixture};
use egress_common::{applied, decisions, probe, prompt, upstream};

/// Every test here runs without confinement.
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

#[test]
fn a_best_effort_policy_is_advisory_and_says_so() {
    let f = fixture();
    let (allowed, denied) = (upstream(), upstream());
    let probe = probe(&f.dir);
    let branch = f
        .task(&prompt(
            &probe,
            &[
                ("proxy", allowed.port),
                ("proxy", denied.port),
                ("direct", denied.port),
            ],
        ))
        .options(options(
            &f,
            network(
                &[format!("127.0.0.1:{}", allowed.port)],
                NetworkEnforce::BestEffort,
            ),
        ))
        .name("advisory")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    assert_eq!(branch.info().status, BranchStatus::NoChanges, "{events:?}");
    let said = text(&events);
    assert!(
        said.contains(&format!("proxy {0}: upstream {0}", allowed.port)),
        "{said}"
    );
    assert!(
        said.contains(&format!("proxy {}: 403", denied.port)),
        "{said}"
    );
    // Advisory: a tool that ignores the proxy is not held to it.
    assert!(
        said.contains(&format!("direct {}: reached", denied.port)),
        "{said}"
    );
    let vars = said
        .split("vars=")
        .nth(1)
        .and_then(|rest| rest.lines().next())
        .unwrap();
    let (url, rest) = vars.split_once(',').unwrap();
    assert!(url.starts_with("http://127.0.0.1:") && url != "http://127.0.0.1:3128");
    assert_eq!(rest, format!("{url},{url},[]"));
    let applied = applied(&events);
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].0, EgressEnforcement::Advisory);
    let reason = applied[0].1.as_deref().unwrap();
    assert!(
        reason.contains("only tools that honor the proxy variables"),
        "{reason}"
    );
    assert!(reason.contains("BRANCHYARD_EGRESS_NETNS=off"), "{reason}");
    let decided: Vec<_> = decisions(&events)
        .into_iter()
        .map(|(_, port, allowed, _)| (port, allowed))
        .collect();
    assert_eq!(decided, [(allowed.port, true), (denied.port, false)]);
}

#[test]
fn a_required_policy_is_refused_where_it_cannot_be_enforced() {
    let f = fixture();
    let refused = f
        .task("say hi")
        .options(options(&f, network(&[], NetworkEnforce::Required)))
        .name("required")
        .run();
    match refused {
        Err(Error::Unsupported(why)) => {
            assert!(why.contains("must be enforced"), "{why}");
            assert!(why.contains("BRANCHYARD_EGRESS_NETNS=off"), "{why}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(f.yard.branch("required").is_err(), "nothing is created");
    // An open policy with nothing to enforce is refused as meaningless.
    let open = Provisioning {
        network: Some(Network::open().with_enforce(NetworkEnforce::Required)),
        ..Provisioning::default()
    };
    match f.task("say hi").options(options(&f, Some(open))).run() {
        Err(Error::Unsupported(why)) => assert!(why.contains("nothing to enforce"), "{why}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn an_open_policy_records_nothing_and_sets_nothing() {
    let f = fixture();
    let branch = f
        .task("say hi")
        .options(options(
            &f,
            Some(Provisioning {
                network: Some(Network::open()),
                ..Provisioning::default()
            }),
        ))
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    assert!(applied(&events).is_empty());
    assert!(decisions(&events).is_empty());
}

/// Serves `github`; packages and indexes it trivially.
#[derive(Debug)]
struct OnePackager;

impl Packager for OnePackager {
    fn served(&self) -> Result<Vec<Bundle>, String> {
        Ok(vec![Bundle {
            id: "github".into(),
            path: PathBuf::from("/bundles/github"),
            hash: "hash-github".into(),
        }])
    }

    fn package(&self, _: &Bundle, out: &Path) -> Result<(), String> {
        fs::create_dir_all(out).unwrap();
        fs::write(out.join("SKILL.md"), "# github\n").map_err(|e| e.to_string())
    }

    fn index(&self, _: &Path, _: &[Bundle], out: &Path) -> Result<(), String> {
        fs::write(out, "# Connectors\n").map_err(|e| e.to_string())
    }
}

/// A grant allows the gateway as the turn gives it: its effect-ledger
/// proxy (docs/effects.md), which reaches the gateway itself from outside
/// the policy. The gateway is not reachable around the ledger.
#[test]
fn a_connector_grant_allows_the_gateway() {
    let f = fixture();
    let (gateway, other) = (upstream(), upstream());
    let url = format!("http://127.0.0.1:{}/mcp", gateway.port);
    f.yard
        .use_connectors(Gateway::local(&f.yard, &url, Arc::new(OnePackager)).unwrap());
    let probe = probe(&f.dir);
    let provision = Provisioning {
        connectors: vec![GrantEntry::parse("github:read").unwrap()],
        network: Some(Network::none()),
        ..Provisioning::default()
    };
    // The port the turn's gateway URL names, read in the harness's shell.
    let given = format!(
        "SH python3 {} proxy $(echo $ANVIL_GATEWAY_URL | sed 's|.*:\\([0-9]*\\)/mcp|\\1|')\n",
        probe.display()
    );
    let branch = f
        .task(&format!(
            "{given}{}",
            prompt(&probe, &[("proxy", gateway.port), ("proxy", other.port)])
        ))
        .options(TaskOptions {
            isolated: true,
            ..options(&f, Some(provision))
        })
        .name("granted")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    let said = text(&events);
    let ledger = events
        .iter()
        .find_map(|e| match &e.activity {
            Activity::Effect(effect) => match effect.as_ref() {
                branchyard::effects::EffectActivity::Proxy { url } => Some(url.clone()),
                _ => None,
            },
            _ => None,
        })
        .expect("the turn's ledger proxy");
    let port: u16 = ledger
        .trim_end_matches("/mcp")
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    // Through the ledger's proxy to the gateway.
    assert!(
        said.contains(&format!("proxy {port}: upstream {}", gateway.port)),
        "{said}"
    );
    assert!(
        said.contains(&format!("proxy {}: 403", gateway.port)),
        "{said}"
    );
    assert!(
        said.contains(&format!("proxy {}: 403", other.port)),
        "{said}"
    );
    let rule = format!("127.0.0.1:{port}");
    assert_eq!(
        decisions(&events),
        [
            ("GET".to_owned(), port, true, Some(rule.clone())),
            ("GET".to_owned(), gateway.port, false, None),
            ("GET".to_owned(), other.port, false, None),
        ]
    );
    // In force for the turn, not stored: the branch's own policy is none.
    let in_force = events.iter().find_map(|e| match &e.activity {
        Activity::Egress(egress) => match egress.as_ref() {
            branchyard::EgressActivity::Applied { policy, allow, .. } => {
                Some((policy.clone(), allow.clone()))
            }
            _ => None,
        },
        _ => None,
    });
    assert_eq!(in_force, Some((rule.clone(), vec![rule])));
    assert_eq!(
        stored_record(&f.root, "granted")["provision"]["network"],
        "none"
    );
}

fn denied(result: Result<impl std::fmt::Debug, Error>, needle: &str) {
    match result {
        Err(Error::Denied(why)) if why.contains(needle) => {}
        other => panic!("expected a denial mentioning {needle:?}, got {other:?}"),
    }
}

fn stored_network(f: &Fixture, name: &str) -> serde_json::Value {
    stored_record(&f.root, name)["provision"]["network"].clone()
}

#[test]
fn a_delegated_child_is_never_wider_than_its_parent() {
    let f = fixture();
    let seat = |network: Option<Network>| Seat {
        harness: "gemini-cli".into(),
        budget: ChildBudget::default(),
        check: None,
        deny: Vec::new(),
        isolated: false,
        provision: Some(Provisioning {
            network,
            ..Provisioning::default()
        }),
        delegates_to: Vec::new(),
        escalates_to: Vec::new(),
        instances: 2,
        bindings: Vec::new(),
    };
    let seats = Seats {
        rig: "team".into(),
        seat: "lead".into(),
        delegates_to: vec!["narrow".into(), "silent".into(), "wide".into()],
        escalates_to: Vec::new(),
        table: [
            (
                "narrow".to_owned(),
                seat(Some(Network::parse_flag("api.example.com:443").unwrap())),
            ),
            ("silent".to_owned(), seat(None)),
            ("wide".to_owned(), seat(Some(Network::open()))),
        ]
        .into_iter()
        .collect(),
    };
    let parent = network(
        &["*.example.com:443".into(), "github.com".into()],
        NetworkEnforce::BestEffort,
    );
    let options = TaskOptions {
        delegation: Some(seats.envelope()),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        seats: Some(seats),
        ..options(&f, parent)
    };
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("lead")
        .run()
        .unwrap();
    let lead = root.delegate(options.clone()).unwrap();
    let by_seat = |seat: &str, name: &str| Spawn {
        seat: Some(seat.into()),
        name: Some(name.into()),
        ..Spawn::new("say hi")
    };
    lead.spawn(by_seat("narrow", "n")).unwrap();
    // A seat that names no policy gets its parent's, never an open one.
    lead.spawn(by_seat("silent", "s")).unwrap();
    denied(lead.spawn(by_seat("wide", "w")), "open network");
    root.wait_subtree().unwrap();
    assert_eq!(
        stored_network(&f, "n"),
        serde_json::json!({"allow": ["api.example.com:443"]})
    );
    assert_eq!(
        stored_network(&f, "s"),
        serde_json::json!({"allow": ["*.example.com:443", "github.com"]})
    );
    assert!(f.yard.branch("w").is_err());
    // A send cannot widen a child either; a narrower one is kept.
    let child = f.yard.branch("n").unwrap();
    let send = |rules: &str| {
        child.send(
            "say hi",
            TaskOptions {
                policy: Policy::allow_all(),
                provision: Some(Provisioning {
                    network: Some(Network::parse_flag(rules).unwrap()),
                    ..Provisioning::default()
                }),
                ..TaskOptions::default()
            },
        )
    };
    denied(send("evil.example.org"), "evil.example.org");
    send("none").unwrap();
    assert_eq!(stored_network(&f, "n"), "none");
}
