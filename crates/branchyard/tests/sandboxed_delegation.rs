//! Delegation from a sandboxed harness, end to end: the fake provider
//! stands in for Microsandbox (its sandboxes share this host's network, so
//! the endpoint is on loopback), the turn's delegation endpoint runs the
//! real `branchyard-mcp` behind MCP's Streamable HTTP, and the fake ACP
//! agent calls it with the URL and bearer token its `mcpServers` name. A
//! harness spawns, waits and hears from a child that reports to it, the
//! child acting only as itself; the token is its turn's alone and never
//! recorded; a sandbox with no address for this host gets no tools.
//! Hermetic; nothing here is evidence about Microsandbox itself.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::path::Path;
use std::sync::Arc;

use branchyard::connectors::{keys, Bundle, Packager};
use branchyard::{
    Activity, Branch, BranchStatus, Envelope, Policy, Provider, SandboxHost, SandboxOptions,
    TaskOptions,
};
use branchyard_runtime::LocalProvider;
use branchyard_sandbox::fake::FakeProvider;
use common::{mcp_server, stored_record, text, Fixture};
use serde_json::{json, Value};

/// A fake provider for the fixture's yard, whose sandboxes reach this host
/// on loopback when `reachable`.
fn fake(f: &Fixture, reachable: bool) {
    let root = f.dir.join("fake-provider");
    let fake = FakeProvider::plain(Box::new(LocalProvider::new()), root);
    f.yard.use_sandbox_provider(Arc::new(fake));
    if reachable {
        f.yard.use_sandbox_host(SandboxHost::loopback());
    }
}

/// Microsandbox, through the fake provider, delegating with the real MCP
/// server.
fn delegating(f: &Fixture) -> TaskOptions {
    TaskOptions {
        provider: Some(Provider::Microsandbox(SandboxOptions {
            image: "registry.example/harness:1".into(),
            ..SandboxOptions::default()
        })),
        delegation: Some(Envelope::default()),
        delegation_server: Some(vec![mcp_server().display().to_string()]),
        policy: Policy::allow_all(),
        ..f.options()
    }
}

/// The text of the branch's last turn.
fn last_text(branch: &Branch) -> String {
    let events = branch.events().unwrap();
    let start = events
        .iter()
        .rposition(|e| matches!(e.activity, Activity::Prompt(_)))
        .unwrap();
    text(&events[start..])
}

/// The MCP server a harness was given, as the fake agent saved it.
fn server(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

/// Its URL and bearer token.
fn endpoint(server: &Value) -> (String, String) {
    assert_eq!(server["type"], "http", "{server}");
    assert_eq!(server["name"], "branchyard", "{server}");
    let headers = server["headers"].as_array().unwrap();
    assert_eq!(headers.len(), 1, "{server}");
    assert_eq!(headers[0]["name"], "Authorization");
    let token = headers[0]["value"]
        .as_str()
        .unwrap()
        .strip_prefix("Bearer ")
        .unwrap();
    (server["url"].as_str().unwrap().to_owned(), token.to_owned())
}

/// Spawn `kid` with `kid_prompt` and wait for it, saving the root's server.
fn spawn_and_wait(saved: &Path, kid_prompt: &str) -> String {
    let spawn = json!({"prompt": kid_prompt, "name": "kid"});
    format!(
        "MCP_SAVE {}\nMCP spawn {spawn}\nMCP wait {}\nMCP inbox {{}}",
        saved.display(),
        json!({"branches": ["kid"]})
    )
}

#[test]
fn a_sandboxed_harness_spawns_waits_and_hears_from_its_child_over_http() {
    let f = Fixture::new();
    fake(&f, true);
    let saved = f.dir.join("root-server.json");
    let kid_saved = f.dir.join("kid-server.json");
    let kid_prompt = format!(
        "MCP_SAVE {}\nMCP report {}\nMCP inspect {}",
        kid_saved.display(),
        json!({"text": "hello from the kid"}),
        json!({"branch": "root"}),
    );
    let root = f
        .yard
        .task(spawn_and_wait(&saved, &kid_prompt))
        .options(delegating(&f))
        .name("root")
        .run()
        .unwrap();
    assert_eq!(
        root.info().status,
        BranchStatus::NoChanges,
        "{:?}",
        root.events()
    );
    let said = last_text(&root);
    for done in ["mcp spawn: ", "mcp wait: ", "mcp inbox: "] {
        assert!(said.contains(done), "{done:?} missing from {said}");
    }
    assert!(!said.contains("mcp spawn error"), "{said}");
    assert!(said.contains("hello from the kid"), "{said}");

    // The child ran in a sandbox of its own, with an endpoint of its own,
    // and acted only as itself: it reported to its parent, and inspecting
    // that parent was refused.
    let kid = f.yard.branch("kid").unwrap();
    assert_eq!(kid.info().parent.as_deref(), Some("root"));
    let kid_said = last_text(&kid);
    assert!(kid_said.contains("mcp report: "), "{kid_said}");
    assert!(kid_said.contains("mcp inspect error: "), "{kid_said}");

    // Each harness was given its turn's endpoint and token: an HTTP MCP
    // server on loopback, its token signed by the yard's key for that
    // endpoint, branch and turn, with no grant, the branch's delegation
    // scope, and an expiry.
    let (url, token) = endpoint(&server(&saved));
    let (kid_url, kid_token) = endpoint(&server(&kid_saved));
    assert!(
        url.starts_with("http://127.0.0.1:") && url.ends_with("/mcp"),
        "{url}"
    );
    assert_ne!(url, kid_url);
    assert_ne!(token, kid_token);
    let jwks: Value = serde_json::from_str(
        &fs::read_to_string(f.root.join(".branchyard/gateway/jwks.json")).unwrap(),
    )
    .unwrap();
    let claims: keys::Claims =
        serde_json::from_value(keys::verify(&jwks, &token).unwrap()).unwrap();
    assert_eq!(
        (
            claims.aud.as_str(),
            claims.by_branch.as_str(),
            claims.by_turn.as_str()
        ),
        (url.as_str(), "root", "1")
    );
    assert!(claims.by_grants.is_empty() && claims.by_purpose.is_none());
    assert_eq!(claims.by_delegation.unwrap().max_depth, 1);
    assert!(claims.exp > claims.iat);
    let kid_claims: keys::Claims =
        serde_json::from_value(keys::verify(&jwks, &kid_token).unwrap()).unwrap();
    assert_eq!(kid_claims.by_branch, "kid");
    assert_eq!(kid_claims.by_delegation.unwrap().max_depth, 0);

    // Neither token is in an event log or a stored record.
    for (branch, secret) in [(&root, &token), (&kid, &kid_token)] {
        let name = &branch.info().name;
        let events = serde_json::to_string(&branch.events().unwrap()).unwrap();
        assert!(!events.contains(secret.as_str()), "{name}'s events");
        let record = stored_record(&f.root, name).to_string();
        assert!(!record.contains(secret.as_str()), "{name}'s record");
    }
}

#[test]
fn the_endpoint_takes_its_own_turns_token_and_no_other() {
    let f = Fixture::new();
    fake(&f, true);
    let saved = f.dir.join("root-server.json");
    let kid_saved = f.dir.join("kid-server.json");
    let kid_prompt = format!("MCP_SAVE {}\nMCP inspect {{}}", kid_saved.display());
    let root = f
        .yard
        .task(spawn_and_wait(&saved, &kid_prompt))
        .options(delegating(&f))
        .name("root")
        .run()
        .unwrap();
    assert_eq!(
        root.info().status,
        BranchStatus::NoChanges,
        "{}",
        last_text(&root)
    );
    let (_, first) = endpoint(&server(&saved));
    let (_, kid) = endpoint(&server(&kid_saved));
    // The root's next turn has a new endpoint and token. Its last turn's
    // token, its child's, and a made-up one are each refused there before
    // anything reaches the engine.
    for (bearer, whose) in [
        (first.as_str(), "the root's last turn"),
        (kid.as_str(), "its child's turn"),
        ("not-a-token", "nobody"),
    ] {
        let prompt = format!("MCP_BEARER {bearer}\nMCP inspect {{}}");
        let root = root.send(&prompt, delegating(&f)).unwrap();
        let said = last_text(&root);
        assert!(
            said.contains("mcp: HTTP 401: the token is not this turn's"),
            "a token of {whose}: {said}"
        );
    }
    // Its own still works.
    let again = f.dir.join("again.json");
    let prompt = format!("MCP_SAVE {}\nMCP inspect {{}}", again.display());
    let root = root.send(&prompt, delegating(&f)).unwrap();
    assert!(
        last_text(&root).contains("mcp inspect: "),
        "{}",
        last_text(&root)
    );
    let (_, latest) = endpoint(&server(&again));
    assert_ne!(latest, first);
}

/// A packager that serves nothing: enough for a gateway's settings.
#[derive(Debug)]
struct Nothing;

impl Packager for Nothing {
    fn served(&self) -> Result<Vec<Bundle>, String> {
        Ok(Vec::new())
    }
    fn package(&self, _: &Bundle, _: &Path) -> Result<(), String> {
        Ok(())
    }
    fn index(&self, _: &Path, _: &[Bundle], _: &Path) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn a_sandbox_with_no_address_for_this_host_gets_no_tools() {
    let f = Fixture::new();
    fake(&f, false);
    assert_eq!(f.yard.sandbox_host(), None);
    // Asked for now: the turn cannot run as asked, and says what to set.
    let root = f
        .yard
        .task("MCP inspect {}")
        .options(delegating(&f))
        .name("root")
        .run()
        .unwrap();
    match &root.info().status {
        BranchStatus::Failed { reason } => assert!(
            reason.starts_with("could not offer delegation: ")
                && reason.contains("effects_sandbox_host"),
            "{reason}"
        ),
        other => panic!("{other:?}"),
    }
    // Kept from that turn: the next runs without the tools, and says so.
    let kept = TaskOptions {
        delegation: None,
        ..delegating(&f)
    };
    let root = root.send("MCP inspect {}", kept).unwrap();
    assert_eq!(root.info().status, BranchStatus::NoChanges);
    assert_eq!(last_text(&root), "mcp: no server\n");
    let warned = root.events().unwrap().into_iter().any(|e| {
        matches!(&e.activity, Activity::Warning(w)
            if w.starts_with("this turn runs without delegation tools: ")
                && w.contains("no address at which a sandbox reaches this host"))
    });
    assert!(warned, "{:?}", root.events());

    // The connector gateway's address for a sandbox to reach its ledger
    // proxy is this host's address for a sandbox, unless one is set.
    let mut gateway = branchyard::connectors::Gateway::local(
        &f.yard,
        "http://127.0.0.1:9/mcp",
        Arc::new(Nothing),
    )
    .unwrap();
    gateway.effects.listen = "0.0.0.0".parse().unwrap();
    gateway.effects.sandbox_host = Some("192.168.127.1".into());
    f.yard.use_connectors(gateway);
    assert_eq!(
        f.yard.sandbox_host(),
        Some(SandboxHost {
            listen: "0.0.0.0".parse().unwrap(),
            host: "192.168.127.1".into(),
        })
    );
    f.yard.use_sandbox_host(SandboxHost::loopback());
    assert_eq!(f.yard.sandbox_host(), Some(SandboxHost::loopback()));
}
