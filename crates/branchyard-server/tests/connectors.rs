//! Connectors on a server: the public keys at `/.well-known/jwks.json`, a
//! request's grant refused without a gateway, a turn's token naming the
//! principal and `<repo>/<branch>`, and the gateway's audit lines recorded
//! as `connector_call` events by the repository's poller. A shell script
//! stands in for `anvil package harness` and `anvil connectors index`; no
//! gateway runs.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use branchyard::connectors::GrantEntry;
use branchyard::{Activity, Provisioning};
use branchyard_server::config::ConnectorsConfig;
use common::{eventually, get, raw, run, task, Fixture, Server};

const FAKE_ANVIL: &str = r##"#!/bin/sh
case "$1 $2" in
"package harness") mkdir -p "$5" && echo "# $(basename "$3")" > "$5/SKILL.md" ;;
"connectors index") echo "# Connectors" > "$6" ;;
*) echo "fake anvil: $*" >&2; exit 2 ;;
esac
"##;

fn connectors(f: &Fixture) -> ConnectorsConfig {
    let bundles = f.dir.join("bundles");
    fs::create_dir_all(bundles.join("github")).unwrap();
    fs::write(bundles.join("github/air.yaml"), "service: github\n").unwrap();
    let anvil = f.dir.join("fake-anvil");
    fs::write(&anvil, FAKE_ANVIL).unwrap();
    fs::set_permissions(&anvil, fs::Permissions::from_mode(0o755)).unwrap();
    ConnectorsConfig {
        gateway: "http://127.0.0.1:9/mcp".into(),
        sandbox_gateway: None,
        issuer: Some("https://by.example".into()),
        signing_key: None,
        audit_file: None,
        bundles,
        anvil: vec![anvil.display().to_string()],
        run_gateway: false,
        listen: None,
        vault_key: None,
        effects_proxy: None,
        effects_listen: None,
        effects_sandbox_host: None,
    }
}

fn granted(prompt: &str, name: &str, grant: &str) -> branchyard_client::api::TaskRequest {
    branchyard_client::api::TaskRequest {
        isolated: true,
        provision: Some(Provisioning {
            connectors: vec![GrantEntry::parse(grant).unwrap()],
            ..Provisioning::default()
        }),
        ..task(prompt, name)
    }
}

#[test]
fn without_a_gateway_there_are_no_keys_and_grants_are_refused() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let (status, _, _) = raw(server.addr, &get("/.well-known/jwks.json", None));
    assert_eq!(status, 404);
    let client = server.client();
    let refused = client
        .repo("app")
        .submit_task(&granted("say hi", "g", "github"), "k1")
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("connectors_not_configured") || refused.contains("no connector gateway"),
        "{refused}"
    );
}

#[test]
fn a_turns_token_names_the_principal_and_the_repository_and_audit_lines_become_events() {
    let f = Fixture::new();
    let mut config = f.config();
    config.connectors = Some(connectors(&f));
    let server = Server::start(config);
    // The public keys need no token, and hold no private part.
    let (status, _, body) = raw(server.addr, &get("/.well-known/jwks.json", None));
    assert_eq!(status, 200, "{body}");
    let jwks: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(jwks["keys"][0]["crv"], "Ed25519");
    assert!(!body.contains("\"d\""));

    let client = server.client();
    let op = run(
        &client,
        &granted(
            "SH echo token=$(cat \"$ANVIL_GATEWAY_TOKEN_FILE\")",
            "granted",
            "github:read",
        ),
    );
    assert!(op.state.is_terminal());
    let events = client.repo("app").events("granted", 0).unwrap().events;
    let said: String = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Harness(branchyard::Event::MessageDelta { text, .. }) => Some(text.clone()),
            _ => None,
        })
        .collect();
    let token = said
        .split("token=")
        .nth(1)
        .and_then(|t| t.lines().next())
        .unwrap_or_else(|| panic!("no token in {said}: {events:?}"))
        .trim();
    let claims = branchyard::connectors::keys::verify(&jwks, token).unwrap();
    assert_eq!(claims["iss"], "https://by.example");
    assert_eq!(claims["aud"], "http://127.0.0.1:9/mcp");
    assert_eq!(claims["sub"], "tester");
    assert_eq!(claims["by_tenant"], "default");
    assert_eq!(claims["by_branch"], "app/granted");

    // The poller records the gateway's lines for this repository only.
    let audit = f.data.join("gateway/audit.jsonl");
    let line = |branch: &str| {
        format!(
            "{{\"time\":\"2026-09-30T12:00:00Z\",\"sub\":\"tester\",\"by_branch\":\"{branch}\",\
             \"connector\":\"github\",\"operation\":\"issues.list\",\"decision\":\"allowed\"}}\n"
        )
    };
    fs::write(&audit, line("app/granted") + &line("other/granted")).unwrap();
    eventually("a connector_call event", || {
        client
            .repo("app")
            .events("granted", 0)
            .unwrap()
            .events
            .iter()
            .filter(|e| matches!(e.activity, Activity::ConnectorCall(_)))
            .count()
            == 1
    });
}
