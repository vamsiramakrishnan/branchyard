//! Connectors through the engine with a fake packager and the fake ACP
//! agent: a granted branch's harness finds its packages, index, gateway
//! URL and a 0600 token signed for its turn; a branch without a grant gets
//! none; a connector the gateway does not serve fails the turn by name; a
//! delegated child's grant is only ever narrower than its parent's; and
//! the gateway's audit log becomes `connector_call` events; two or more
//! granted connectors also get Anvil's composite, and a composite that
//! cannot be written is a warning, not the turn's failure. Hermetic: no
//! gateway runs and nothing is called.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::unwrap_used
)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use branchyard::connectors::{self, Bundle, Gateway, GrantEntry, Packager};
use branchyard::{
    Activity, BranchStatus, Budget, Envelope, Error, Policy, Provisioning, Spawn, TaskOptions,
};
use common::{fake_agent, stored_record, text, Fixture};

const URL: &str = "http://127.0.0.1:9/mcp";

/// Serves the bundles it names; packages each as a SKILL.md naming it, and
/// indexes a grant by listing its entries.
#[derive(Debug, Default)]
struct FakePackager {
    bundles: Vec<&'static str>,
    packaged: AtomicUsize,
    composed: AtomicUsize,
    /// Fail `compose`, as an Anvil without `connectors compose` does.
    cannot_compose: bool,
}

impl Packager for FakePackager {
    fn served(&self) -> Result<Vec<Bundle>, String> {
        Ok(self
            .bundles
            .iter()
            .map(|id| Bundle {
                id: (*id).to_owned(),
                path: PathBuf::from(format!("/bundles/{id}")),
                hash: format!("hash-{}", id.replace('/', "-")),
            })
            .collect())
    }

    fn package(&self, bundle: &Bundle, out: &Path) -> Result<(), String> {
        self.packaged.fetch_add(1, Ordering::SeqCst);
        fs::create_dir_all(out.join("bin")).unwrap();
        fs::write(out.join("SKILL.md"), format!("# {}\n", bundle.id)).unwrap();
        fs::write(out.join("bin").join(&bundle.id), "#!/bin/sh\n").unwrap();
        Ok(())
    }

    fn index(&self, grants: &Path, bundles: &[Bundle], out: &Path) -> Result<(), String> {
        let grants: Vec<GrantEntry> =
            serde_json::from_str(&fs::read_to_string(grants).unwrap()).unwrap();
        let mut text = String::from("# Connectors\n");
        for bundle in bundles {
            text.push_str(&format!("- {}: {}/SKILL.md\n", bundle.id, bundle.id));
        }
        text.push_str(&format!("grants: {}\n", connectors_text(&grants)));
        fs::write(out, text).map_err(|e| e.to_string())
    }

    fn compose(&self, bundles: &[Bundle], packages: &Path, out: &Path) -> Result<(), String> {
        self.composed.fetch_add(1, Ordering::SeqCst);
        if self.cannot_compose {
            fs::create_dir_all(out).unwrap();
            fs::write(out.join("partial"), "").unwrap();
            return Err("error: unknown command 'compose'".into());
        }
        // The packages are placed before the composite is asked for.
        for bundle in bundles {
            assert!(
                packages.join(&bundle.id).join("SKILL.md").is_file(),
                "{bundle:?}"
            );
        }
        fs::create_dir_all(out).unwrap();
        let ids: Vec<&str> = bundles.iter().map(|b| b.id.as_str()).collect();
        fs::write(
            out.join("SKILL.md"),
            format!("compose: {}\n", ids.join(" ")),
        )
        .map_err(|e| e.to_string())
    }
}

fn connectors_text(grants: &[GrantEntry]) -> String {
    grants
        .iter()
        .map(|g| g.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn gateway(f: &Fixture, bundles: &[&'static str]) -> Arc<FakePackager> {
    let packager = Arc::new(FakePackager {
        bundles: bundles.to_vec(),
        ..FakePackager::default()
    });
    f.yard
        .use_connectors(Gateway::local(&f.yard, URL, packager.clone()).expect("a local gateway"));
    packager
}

fn granted(f: &Fixture, grants: &[&str]) -> TaskOptions {
    TaskOptions {
        isolated: true,
        provision: Some(Provisioning {
            connectors: grants
                .iter()
                .map(|g| GrantEntry::parse(g).unwrap())
                .collect(),
            ..Provisioning::default()
        }),
        policy: Policy::allow_all(),
        ..f.options()
    }
}

fn jwks(f: &Fixture) -> serde_json::Value {
    let path = f.root.join(".branchyard/gateway/jwks.json");
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// The first line after `marker` in `said`.
fn after<'a>(said: &'a str, marker: &str) -> &'a str {
    said.split(marker)
        .nth(1)
        .unwrap_or_else(|| panic!("no {marker} in {said}"))
        .lines()
        .next()
        .unwrap()
        .trim()
}

#[test]
fn a_granted_turn_gets_packages_an_index_and_a_signed_token() {
    let f = Fixture::new();
    let packager = gateway(&f, &["github", "linear"]);
    let branch = f
        .task(
            "SH echo url=$ANVIL_GATEWAY_URL\n\
             SH stat -c 'mode=%a' \"$ANVIL_GATEWAY_TOKEN_FILE\"\n\
             SH echo token=$(cat \"$ANVIL_GATEWAY_TOKEN_FILE\")\n\
             SH ls \"$HOME/.branchyard/connectors\" | tr '\\n' ' ' | sed 's/^/placed=/'; echo\n\
             SH cat \"$HOME/.branchyard/connectors/INDEX.md\"\n\
             SH cat \"$HOME/.branchyard/connectors/github/SKILL.md\"",
        )
        .options(TaskOptions {
            budget: Budget {
                max_duration: Some(Duration::from_secs(120)),
                ..Budget::default()
            },
            ..granted(&f, &["github:read:issues.*"])
        })
        .name("granted")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    assert_eq!(branch.info().status, BranchStatus::NoChanges, "{events:?}");
    let said = text(&events);
    // The harness reaches the gateway through its turn's effect-ledger
    // proxy on loopback (docs/effects.md); the token's audience stays the
    // gateway's own URL.
    let given = after(&said, "url=");
    assert!(
        given.starts_with("http://127.0.0.1:") && given.ends_with("/mcp") && given != URL,
        "{given}"
    );
    assert_eq!(after(&said, "mode="), "600");
    // Only the granted connector is placed, with the index.
    assert_eq!(after(&said, "placed="), "INDEX.md github");
    assert!(said.contains("- github: github/SKILL.md"), "{said}");
    assert!(said.contains("grants: github:read:issues.*"), "{said}");
    assert!(said.contains("# github"), "{said}");

    // The token verifies against the yard's public keys, with the
    // contract's claims.
    let token = after(&said, "token=");
    let claims = connectors::keys::verify(&jwks(&f), token).expect("a valid token");
    let issuer = claims["iss"].as_str().unwrap();
    assert!(issuer.starts_with("branchyard:local:"), "{claims}");
    assert_eq!(claims["aud"], URL);
    assert!(claims["sub"].as_str().unwrap().starts_with("local:"));
    assert_eq!(claims["by_tenant"], "local");
    assert_eq!(claims["by_branch"], "granted");
    assert_eq!(claims["by_turn"], "1");
    assert_eq!(
        claims["by_grants"],
        serde_json::json!([{"connector": "github", "operations": ["issues.*"],
                            "mode": "read"}])
    );
    // A turn's token is never a connect token: the gateway refuses it at
    // its connect routes.
    assert!(claims.get("by_purpose").is_none(), "{claims}");
    let (iat, exp) = (
        claims["iat"].as_u64().unwrap(),
        claims["exp"].as_u64().unwrap(),
    );
    // At most the turn's deadline (two minutes), never over an hour.
    assert!(exp > iat && exp <= iat + 121, "{claims}");
    assert!(!claims["jti"].as_str().unwrap().is_empty());

    // Recorded by name; the token file is gone once the turn ended.
    let provisioned = events
        .iter()
        .find_map(|e| match &e.activity {
            Activity::Provisioned {
                connectors,
                env,
                files,
                ..
            } => Some((connectors.clone(), env.clone(), files.clone())),
            _ => None,
        })
        .expect("a provisioned event");
    assert_eq!(provisioned.0, ["github"]);
    assert!(provisioned.1.contains(&"ANVIL_GATEWAY_URL".to_owned()));
    assert!(provisioned
        .1
        .contains(&"ANVIL_GATEWAY_TOKEN_FILE".to_owned()));
    assert!(provisioned
        .2
        .contains(&".branchyard/gateway-token".to_owned()));
    let home = PathBuf::from(stored_record(&f.root, "granted")["home"].as_str().unwrap());
    assert!(!home.join(".branchyard/gateway-token").exists());
    assert!(home.join(".branchyard/connectors/INDEX.md").is_file());
    // The yard's key is private.
    let key = f.root.join(".branchyard/gateway/key");
    assert_eq!(
        fs::metadata(&key).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // A second turn reuses the cached package and signs a new token.
    let again = branch
        .send(
            "SH echo token=$(cat \"$ANVIL_GATEWAY_TOKEN_FILE\")",
            TaskOptions {
                policy: Policy::allow_all(),
                ..TaskOptions::default()
            },
        )
        .unwrap();
    let said = text(&again.events().unwrap());
    let tokens: Vec<&str> = said
        .split("token=")
        .skip(1)
        .map(|t| t.lines().next().unwrap().trim())
        .collect();
    assert_eq!(tokens.len(), 2, "{said}");
    assert_ne!(tokens[0], tokens[1], "a new token per turn");
    let last = tokens[1];
    let claims = connectors::keys::verify(&jwks(&f), last).unwrap();
    assert_eq!(claims["by_turn"], "2");
    assert_eq!(
        packager.packaged.load(Ordering::SeqCst),
        1,
        "cached by bundle hash"
    );
}

#[test]
fn a_connect_token_names_the_person_for_the_connect_routes_only() {
    let f = Fixture::new();
    let gw = Gateway::local(&f.yard, URL, Arc::new(FakePackager::default())).unwrap();
    // Asked for an hour, it still lives at most ten minutes.
    let token = gw
        .connect_token(None, Duration::from_secs(3600))
        .expect("a connect token");
    let claims = connectors::keys::verify(&jwks(&f), &token).expect("a valid token");
    assert_eq!(claims["by_purpose"], "connect");
    assert_eq!(claims["by_purpose"], connectors::keys::CONNECT_PURPOSE);
    assert_eq!(claims["by_grants"], serde_json::json!([]));
    assert_eq!(claims["by_branch"], "");
    assert_eq!(claims["by_turn"], "");
    assert_eq!(claims["aud"], URL);
    assert!(claims["sub"].as_str().unwrap().starts_with("local:"));
    let (iat, exp) = (
        claims["iat"].as_u64().unwrap(),
        claims["exp"].as_u64().unwrap(),
    );
    assert!(
        exp > iat && exp - iat <= connectors::CONNECT_TTL.as_secs(),
        "{claims}"
    );
    let named = gw
        .connect_token(Some("local:ada"), Duration::from_secs(60))
        .unwrap();
    let named = connectors::keys::verify(&jwks(&f), &named).unwrap();
    assert_eq!(named["sub"], "local:ada");
    // A person's granted token (a call Branchyard makes as them) is a tool
    // caller, not a connect token.
    let granted = gw
        .person_token_granted(vec![GrantEntry::read("github")], Duration::from_secs(60))
        .unwrap();
    let granted = connectors::keys::verify(&jwks(&f), &granted).unwrap();
    assert!(granted.get("by_purpose").is_none(), "{granted}");
}

#[test]
fn the_token_never_reaches_the_log_or_the_record_and_the_index_is_in_the_instructions() {
    let f = Fixture::new();
    gateway(&f, &["github"]);
    let branch = f
        .task("SHOW_INSTRUCTIONS")
        .options(granted(&f, &["github"]))
        .name("quiet")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    let said = text(&events);
    assert!(
        said.contains(".branchyard/connectors/INDEX.md") && said.contains("Connectors (github)"),
        "{said}"
    );
    let log = serde_json::to_string(&events).unwrap();
    let record = stored_record(&f.root, "quiet").to_string();
    // A token is three base64url parts; neither the log nor the record
    // holds anything signed with the key.
    for text in [&log, &record] {
        assert!(!text.contains("eyJhbGciOiJFZERTQSI"), "{text}");
    }
}

#[test]
fn two_granted_connectors_also_get_the_composite() {
    let f = Fixture::new();
    let packager = gateway(&f, &["github", "linear", "slack"]);
    let branch = f
        .task(
            "SH ls \"$HOME/.branchyard/connectors\" | tr '\\n' ' ' | sed 's/^/placed=/'; echo\n\
             SH cat \"$HOME/.branchyard/connectors/_compose/SKILL.md\"",
        )
        .options(granted(&f, &["github:read", "linear:write"]))
        .name("composed")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    assert_eq!(branch.info().status, BranchStatus::NoChanges, "{events:?}");
    let said = text(&events);
    assert_eq!(after(&said, "placed="), "INDEX.md _compose github linear");
    // Composed over exactly the granted connectors.
    assert!(said.contains("compose: github linear"), "{said}");
    assert_eq!(packager.composed.load(Ordering::SeqCst), 1);

    let instructed = f
        .task("SHOW_INSTRUCTIONS")
        .options(granted(&f, &["github", "slack"]))
        .name("instructed")
        .run()
        .unwrap();
    let said = text(&instructed.events().unwrap());
    assert!(
        said.contains(".branchyard/connectors/_compose/SKILL.md composes them"),
        "{said}"
    );
    assert_eq!(packager.composed.load(Ordering::SeqCst), 2);

    // One connector: nothing to compose, and the instructions do not offer it.
    let single = f
        .task("SHOW_INSTRUCTIONS")
        .options(granted(&f, &["github"]))
        .name("single")
        .run()
        .unwrap();
    let said = text(&single.events().unwrap());
    assert!(!said.contains("_compose"), "{said}");
    assert_eq!(packager.composed.load(Ordering::SeqCst), 2);
}

#[test]
fn a_composite_anvil_cannot_write_is_a_warning_not_a_failure() {
    let f = Fixture::new();
    let packager = Arc::new(FakePackager {
        bundles: vec!["github", "linear"],
        cannot_compose: true,
        ..FakePackager::default()
    });
    f.yard
        .use_connectors(Gateway::local(&f.yard, URL, packager.clone()).expect("a local gateway"));
    let branch = f
        .task("SH ls \"$HOME/.branchyard/connectors\" | tr '\\n' ' ' | sed 's/^/placed=/'; echo")
        .options(granted(&f, &["github", "linear"]))
        .name("uncomposed")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    assert_eq!(branch.info().status, BranchStatus::NoChanges, "{events:?}");
    let said = text(&events);
    // The connectors are placed one by one, with nothing left of the attempt.
    assert_eq!(after(&said, "placed="), "INDEX.md github linear");
    assert!(!said.contains("_compose"), "{said}");
    let warning = events
        .iter()
        .find_map(|e| match &e.activity {
            Activity::Warning(text) if text.contains("composite") => Some(text.clone()),
            _ => None,
        })
        .expect("a warning about the composite");
    assert!(warning.contains("unknown command 'compose'"), "{warning}");
    assert_eq!(packager.composed.load(Ordering::SeqCst), 1);
}

#[test]
fn a_branch_without_a_grant_gets_no_token_or_variables() {
    let f = Fixture::new();
    gateway(&f, &["github"]);
    let branch = f
        .task("SH echo url=[$ANVIL_GATEWAY_URL] file=[$ANVIL_GATEWAY_TOKEN_FILE]")
        .options(TaskOptions {
            isolated: true,
            ..f.options()
        })
        .name("plain")
        .run()
        .unwrap();
    let said = text(&branch.events().unwrap());
    assert!(said.contains("url=[] file=[]"), "{said}");
    let home = PathBuf::from(stored_record(&f.root, "plain")["home"].as_str().unwrap());
    assert!(!home.join(".branchyard/connectors").exists());
}

#[test]
fn a_connector_the_gateway_does_not_serve_fails_the_turn_by_name() {
    let f = Fixture::new();
    gateway(&f, &["github"]);
    let branch = f
        .task("say hi")
        .options(granted(&f, &["github", "slack:write"]))
        .name("unserved")
        .run()
        .unwrap();
    match &branch.info().status {
        BranchStatus::Failed { reason } => {
            assert!(reason.contains("connector slack is not served"), "{reason}");
            assert!(reason.contains("it serves: github"), "{reason}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
}

#[test]
fn a_grant_needs_a_gateway_and_a_private_home() {
    let f = Fixture::new();
    // No gateway configured: the turn fails, naming what to set.
    let branch = f
        .task("say hi")
        .options(granted(&f, &["github"]))
        .name("nogateway")
        .run()
        .unwrap();
    match &branch.info().status {
        BranchStatus::Failed { reason } => {
            assert!(reason.contains("[connectors] gateway"), "{reason}")
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    // Not isolated: refused before anything is created.
    gateway(&f, &["github"]);
    let refused = f
        .task("say hi")
        .options(TaskOptions {
            isolated: false,
            ..granted(&f, &["github"])
        })
        .name("shared")
        .run();
    match refused {
        Err(Error::Unsupported(why)) => assert!(why.contains("private"), "{why}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(f.yard.branch("shared").is_err());
}

fn denied(result: Result<impl std::fmt::Debug, Error>, needle: &str) {
    match result {
        Err(Error::Denied(why)) if why.contains(needle) => {}
        other => panic!("expected a denial mentioning {needle:?}, got {other:?}"),
    }
}

fn child_grant(f: &Fixture, name: &str) -> Vec<String> {
    let record = stored_record(&f.root, name);
    record["provision"]["connectors"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .map(|e| {
                    serde_json::from_value::<GrantEntry>(e.clone())
                        .unwrap()
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_delegated_childs_grant_is_only_ever_narrower() {
    let f = Fixture::new();
    gateway(&f, &["github", "linear", "slack"]);
    let options = TaskOptions {
        delegation: Some(Envelope::depth(2)),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        ..granted(&f, &["github:write:issues.*", "linear:read"])
    };
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options.clone()).unwrap();
    let spawn = |name: &str, grants: Option<&[&str]>| Spawn {
        prompt: "say hi".into(),
        name: Some(name.into()),
        connectors: grants.map(|g| g.iter().map(|e| GrantEntry::parse(e).unwrap()).collect()),
        ..Spawn::default()
    };
    // Nothing asked: the parent's grant.
    delegate.spawn(spawn("inherits", None)).unwrap();
    // Wider asks are cut to the parent's.
    delegate
        .spawn(spawn(
            "narrowed",
            Some(&["github:write+confirm", "linear:write"]),
        ))
        .unwrap();
    // Narrower asks are kept.
    delegate
        .spawn(spawn("narrower", Some(&["github:read:issues.list"])))
        .unwrap();
    // Nothing in common with the parent's: refused by name.
    denied(
        delegate.spawn(spawn("wider", Some(&["slack:read"]))),
        "slack:read",
    );
    denied(
        delegate.spawn(spawn("other-ops", Some(&["github:read:pulls.*"]))),
        "pulls.*",
    );
    root.wait_subtree().unwrap();
    assert_eq!(
        child_grant(&f, "inherits"),
        ["github:write:issues.*", "linear:read"]
    );
    assert_eq!(
        child_grant(&f, "narrowed"),
        ["github:write:issues.*", "linear:read"]
    );
    assert_eq!(child_grant(&f, "narrower"), ["github:read:issues.list"]);
    assert!(f.yard.branch("wider").is_err());

    // A grandchild is bounded by its own parent, not the root.
    let child = f
        .yard
        .branch("narrower")
        .unwrap()
        .delegate(options.clone())
        .unwrap();
    denied(
        child.spawn(spawn("gc-wide", Some(&["linear:read"]))),
        "linear:read",
    );
    child
        .spawn(spawn("gc", Some(&["github:write:issues.*"])))
        .unwrap();
    f.yard.branch("narrower").unwrap().wait_subtree().unwrap();
    assert_eq!(child_grant(&f, "gc"), ["github:read:issues.list"]);

    // A send cannot widen a delegated child's grant either.
    let widened = f.yard.branch("narrower").unwrap().send(
        "say hi",
        TaskOptions {
            provision: Some(Provisioning {
                connectors: vec![GrantEntry::parse("slack:read").unwrap()],
                ..Provisioning::default()
            }),
            policy: Policy::allow_all(),
            ..TaskOptions::default()
        },
    );
    denied(widened, "slack:read");
    assert_eq!(child_grant(&f, "narrower"), ["github:read:issues.list"]);
}

#[test]
fn audit_lines_become_connector_call_events_once() {
    let f = Fixture::new();
    gateway(&f, &["github"]);
    let branch = f
        .task("say hi")
        .options(granted(&f, &["github"]))
        .name("audited")
        .run()
        .unwrap();
    let audit = f.root.join(".branchyard/gateway/audit.jsonl");
    let line = |branch: &str, op: &str, decision: &str| {
        format!(
            "{{\"time\":\"2026-09-30T12:00:00Z\",\"sub\":\"local:me\",\"by_tenant\":\"local\",\
             \"by_branch\":\"{branch}\",\"by_turn\":\"1\",\"connector\":\"github\",\
             \"operation\":\"{op}\",\"decision\":\"{decision}\",\"upstream_status\":200,\
             \"latency_ms\":12}}\n"
        )
    };
    fs::write(
        &audit,
        format!(
            "{}{}{}not json\n{}",
            line("audited", "issues.list", "allowed"),
            line("someone-else", "issues.list", "allowed"),
            line("audited", "issues.create", "denied"),
            // A line still being written is left for later.
            "{\"by_branch\":\"audited\""
        ),
    )
    .unwrap();
    let calls = |f: &Fixture| -> Vec<connectors::ConnectorCall> {
        f.yard
            .branch("audited")
            .unwrap()
            .events()
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.activity {
                Activity::ConnectorCall(call) => Some(*call),
                _ => None,
            })
            .collect()
    };
    assert_eq!(f.yard.ingest_connector_audit().unwrap(), 2);
    assert_eq!(f.yard.ingest_connector_audit().unwrap(), 0, "read once");
    let recorded = calls(&f);
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].operation, "issues.list");
    assert_eq!(recorded[1].decision, "denied");
    assert_eq!(recorded[0].upstream_status, Some(200));
    let events = branch.events().unwrap();
    let at = events
        .iter()
        .find(|e| matches!(e.activity, Activity::ConnectorCall(_)))
        .unwrap()
        .at_ms;
    assert_eq!(at, 1_790_769_600_000, "the line's own time");
    // The unfinished line completes.
    let mut text = fs::read_to_string(&audit).unwrap();
    text.push_str(
        ",\"connector\":\"github\",\"operation\":\"pulls.list\",\"decision\":\"allowed\"}\n",
    );
    fs::write(&audit, text).unwrap();
    assert_eq!(f.yard.ingest_connector_audit().unwrap(), 1);
    // A new log (rotated) is read from its start.
    fs::remove_file(&audit).unwrap();
    fs::write(&audit, line("audited", "issues.get", "allowed")).unwrap();
    assert_eq!(f.yard.ingest_connector_audit().unwrap(), 1);
    assert_eq!(calls(&f).len(), 4);
}

#[test]
fn a_sandboxed_turn_is_given_the_gateway_as_the_sandbox_reaches_it() {
    use branchyard::{Provider, SandboxOptions};
    let f = Fixture::new();
    let fake = std::sync::Arc::new(branchyard_sandbox::fake::FakeProvider::plain(
        Box::new(branchyard_runtime::LocalProvider::new()),
        f.dir.join("fake-provider"),
    ));
    f.yard.use_sandbox_provider(fake);
    let packager = Arc::new(FakePackager {
        bundles: vec!["github"],
        ..FakePackager::default()
    });
    let sandboxed = |name: &str| TaskOptions {
        provider: Some(Provider::Microsandbox(SandboxOptions {
            image: "registry.example/harness:1".into(),
            ..SandboxOptions::default()
        })),
        isolated: false,
        name: Some(name.into()),
        ..granted(&f, &["github"])
    };
    // No address a sandbox reaches the gateway at: the turn fails, naming
    // the setting.
    f.yard
        .use_connectors(Gateway::local(&f.yard, URL, packager.clone()).unwrap());
    let branch = f
        .task("SH echo url=$ANVIL_GATEWAY_URL")
        .options(sandboxed("nowhere"))
        .run()
        .unwrap();
    match &branch.info().status {
        BranchStatus::Failed { reason } => assert!(reason.contains("sandbox_gateway"), "{reason}"),
        other => panic!("expected a failure, got {other:?}"),
    }
    // With one, the harness gets it and the token file as it sees its home.
    let mut gateway = Gateway::local(&f.yard, URL, packager).unwrap();
    gateway.sandbox_url = Some("http://192.168.127.1:8931/mcp".into());
    f.yard.use_connectors(gateway);
    let branch = f
        .task("SH echo url=$ANVIL_GATEWAY_URL file=$ANVIL_GATEWAY_TOKEN_FILE")
        .options(sandboxed("boxed"))
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    let said = text(&events);
    let given = after(&said, "url=");
    assert!(
        given.starts_with("http://192.168.127.1:8931/mcp file="),
        "{given}"
    );
    // The guest's `/branchyard/home`, which the fake provider runs at its
    // host directory.
    assert!(given.ends_with("/.branchyard/gateway-token"), "{given}");
    // The token's audience is still the gateway's canonical URL.
    let home = PathBuf::from(stored_record(&f.root, "boxed")["home"].as_str().unwrap());
    assert!(home
        .join(".branchyard/connectors/github/SKILL.md")
        .is_file());
}

#[test]
fn a_seats_grant_is_narrowed_to_its_parents_too() {
    use branchyard::{ChildBudget, Seat, Seats};
    let f = Fixture::new();
    gateway(&f, &["github", "linear"]);
    let seat = |grants: &[&str]| Seat {
        harness: "gemini-cli".into(),
        budget: ChildBudget::default(),
        check: None,
        deny: Vec::new(),
        isolated: false,
        provision: Some(Provisioning {
            connectors: grants
                .iter()
                .map(|g| GrantEntry::parse(g).unwrap())
                .collect(),
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
        delegates_to: vec!["writer".into(), "none".into(), "outsider".into()],
        escalates_to: Vec::new(),
        table: [
            ("writer".to_owned(), seat(&["github:write+confirm"])),
            ("none".to_owned(), seat(&[])),
            ("outsider".to_owned(), seat(&["linear:read"])),
        ]
        .into_iter()
        .collect(),
    };
    let options = TaskOptions {
        delegation: Some(seats.envelope()),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        seats: Some(seats),
        ..granted(&f, &["github:read"])
    };
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("lead")
        .run()
        .unwrap();
    let lead = root.delegate(options).unwrap();
    let by_seat = |seat: &str, name: &str| Spawn {
        seat: Some(seat.into()),
        name: Some(name.into()),
        ..Spawn::new("say hi")
    };
    lead.spawn(by_seat("writer", "w")).unwrap();
    // A seat without connectors gets none, not its parent's.
    lead.spawn(by_seat("none", "n")).unwrap();
    denied(lead.spawn(by_seat("outsider", "o")), "linear:read");
    // A spawn's own ask still narrows within the seat's parent.
    lead.spawn(Spawn {
        connectors: Some(vec![GrantEntry::parse("github:read:issues.*").unwrap()]),
        ..by_seat("writer", "w2")
    })
    .unwrap();
    root.wait_subtree().unwrap();
    assert_eq!(child_grant(&f, "w"), ["github:read"]);
    assert!(child_grant(&f, "n").is_empty());
    assert_eq!(child_grant(&f, "w2"), ["github:read:issues.*"]);
}
