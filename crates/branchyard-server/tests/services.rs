//! The fleet's service registry: the registry conformance on the server's
//! operation store (SQLite here; PostgreSQL in `tests/postgres.rs`), and
//! over real HTTP: `GET /.well-known/branchyard` without a token, a model
//! gateway registering itself and being found by capability, who may
//! register and deregister what, workers listed as services, and the
//! server announcing itself in the served repository's own registry.

mod common;

use std::sync::Arc;

use branchyard::services::{
    conformance, Capability, Endpoint, LocalRegistry, Query, ServiceState, ServiceStore,
    KIND_SERVER, KIND_WORKER,
};
use branchyard_client::api::RegisterServiceRequest;
use branchyard_server::config::{Principal, Token};
use branchyard_server::store::{MemoryStore, OperationStore, SqliteStore};
use common::{get, post, raw, Fixture, Server, TOKEN};

#[test]
fn the_operation_stores_registry_conforms() {
    let memory = MemoryStore::default();
    conformance::check(memory.services(), "server memory");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let file = SqliteStore::open(&path, None).unwrap();
    conformance::check(file.services(), "server sqlite");
    let other = tempfile::tempdir().unwrap();
    let path = other.path().join("state.db");
    let stores: Vec<Arc<dyn ServiceStore>> = (0..3)
        .map(|_| Arc::new(SqliteStore::open(&path, None).unwrap()) as Arc<dyn ServiceStore>)
        .collect();
    conformance::check_concurrent(stores, 20, "server sqlite handles");
}

fn gateway(id: &str) -> RegisterServiceRequest {
    RegisterServiceRequest {
        id: Some(id.into()),
        kind: "model_gateway".into(),
        capabilities: [
            (
                "models".to_owned(),
                Capability::List(vec!["claude-opus".into(), "gpt-5".into()]),
            ),
            ("protocol".to_owned(), Capability::Text("anthropic".into())),
        ]
        .into_iter()
        .collect(),
        endpoints: vec![Endpoint::url("http://127.0.0.1:7777/v1")],
        ttl_seconds: Some(60),
        ..RegisterServiceRequest::default()
    }
}

#[test]
fn a_server_describes_itself_and_its_services_and_takes_registrations() {
    let f = Fixture::new();
    let mut config = f.config();
    config.tokens.push(Token {
        name: "reader".into(),
        secret: "reader-token-0123456789".into(),
    });
    config.principals.insert(
        "reader".into(),
        Principal {
            name: "reader".into(),
            tenant: "default".into(),
            scopes: ["read"].into_iter().map(String::from).collect(),
            repos: None,
        },
    );
    let server = Server::start(config);
    let client = server.client();

    // Public: no token needed, and it says what it is.
    let (status, _, body) = raw(server.addr, &get("/.well-known/branchyard", None));
    assert_eq!(status, 200, "{body}");
    let known: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(known["service"], "branchyard");
    assert_eq!(known["api"], "/v1");
    assert_eq!(known["services_uri"], "/v1/services");
    assert_eq!(known["repos"], serde_json::json!(["app"]));
    assert!(known.get("jwks_uri").is_none(), "no connectors here");
    let kinds: Vec<&str> = known["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&KIND_SERVER), "{kinds:?}");
    // Endpoints and owners are not public.
    assert!(!body.contains("127.0.0.1:"), "{body}");

    // The server, and its dispatcher as a worker, are in the fleet's list.
    let listed = client.services(None).unwrap();
    let own = listed
        .iter()
        .find(|s| s.kind == KIND_SERVER)
        .expect("the server's record");
    assert_eq!(own.url(), Some(server.url().as_str()));
    assert_eq!(
        own.capability("repos"),
        Some(&Capability::List(vec!["app".into()]))
    );
    assert!(listed.iter().any(|s| s.kind == KIND_WORKER), "{listed:?}");

    // A model gateway registers itself and is found by what it serves.
    let registered = client.register_service(&gateway("mg-1")).unwrap();
    assert_eq!(registered.state, ServiceState::Live);
    assert_eq!(registered.owner.principal.as_deref(), Some("tester"));
    assert!(registered.reclaim.is_none());
    let found = client.services(Some("model_gateway")).unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].answers(&Query::kind("model_gateway").require("models", "gpt-5")));
    assert_eq!(found[0].url(), Some("http://127.0.0.1:7777/v1"));
    let (_, _, body) = raw(server.addr, &get("/.well-known/branchyard", None));
    assert!(body.contains("\"model_gateway\""), "{body}");
    // Registering again renews, keeping when it first registered.
    let renewed = client.register_service(&gateway("mg-1")).unwrap();
    assert_eq!(renewed.registered_ms, registered.registered_ms);
    assert!(renewed.seq > registered.seq);

    // A caller cannot ask the server to reclaim anything: a `reclaim` in
    // the body is not part of the request, and is dropped.
    let body = r#"{"id":"evil","kind":"mcp_server","reclaim":{"type":"process","host":"h","pid":1,"start":"1"}}"#;
    let (status, _, text) = raw(server.addr, &post("/v1/services", Some(TOKEN), "", body));
    assert_eq!(status, 200, "{text}");
    let evil: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(evil.get("reclaim").is_none(), "{text}");

    // Workers are not registered over the API; they claim work.
    let mut worker = gateway("w");
    worker.kind = KIND_WORKER.into();
    assert!(client.register_service(&worker).is_err());

    // `read` lists but does not register or deregister.
    let reader = branchyard_client::Client::new(&server.url(), "reader-token-0123456789").unwrap();
    assert_eq!(reader.services(Some("model_gateway")).unwrap().len(), 1);
    let denied = reader.register_service(&gateway("mg-2")).unwrap_err();
    assert!(denied.to_string().contains("admin"), "{denied}");
    assert!(reader.deregister_service("mg-1").is_err());

    // Its registrant deregisters it.
    let left = client.deregister_service("mg-1").unwrap();
    assert_eq!(left.state, ServiceState::Left);
    assert!(client
        .services(Some("model_gateway"))
        .unwrap()
        .iter()
        .all(|s| s.state == ServiceState::Left));
    assert!(client.deregister_service("mg-1").is_err());
    assert!(client.reclaim_services().unwrap().is_empty());

    // On this machine, the repository's own registry finds the server, so
    // a `by` here finds it without being told where it is.
    let local = LocalRegistry::open(f.root.join(".branchyard/registry.db")).unwrap();
    let here = branchyard::services::resolve(
        &local,
        &Query::kind(KIND_SERVER).require("repo", "app"),
        branchyard_support::time::now_ms(),
    )
    .unwrap()
    .expect("the server, in the repository's registry");
    assert_eq!(here.url(), Some(server.url().as_str()));
    let id = here.id.clone();
    server.stop();
    // Stopping deregisters it.
    assert_eq!(local.get(&id).unwrap().unwrap().state, ServiceState::Left);
}
