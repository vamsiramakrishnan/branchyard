//! A server whose model backend is an AIPlex access point called with a
//! token the server signs (`auth = "turn-token"`): its public keys served
//! at `/.well-known/jwks.json` without `connectors`, and a turn's token
//! verifying against them, issued as `public_url`. `MockHttp` plays the
//! access point; a Python script the fake agent runs is the harness.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;

use base64::Engine as _;
use branchyard::models::ModelAccess;
use branchyard::Provisioning;
use branchyard_setup::config::{ModelBackendConfig, ModelRouteConfig, ModelsConfig};
use branchyard_testkit::{MockHttp, Response};
use common::{get, raw, run, task, Fixture, Server};
use serde_json::{json, Value};

const HARNESS: &str = r#"import json, os, urllib.request
body = json.dumps({"model": "claude-sonnet-4-6", "max_tokens": 8,
    "messages": [{"role": "user", "content": "hi"}]}).encode()
request = urllib.request.Request(os.environ["ANTHROPIC_BASE_URL"] + "/v1/messages", data=body,
    headers={"x-api-key": os.environ["ANTHROPIC_API_KEY"], "anthropic-version": "2023-06-01",
        "content-type": "application/json"}, method="POST")
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
with opener.open(request, timeout=60) as response:
    response.read()
    print("called", response.status)
"#;

fn signed(access: &MockHttp) -> ModelsConfig {
    let backend = ModelBackendConfig {
        api: "anthropic".into(),
        url: Some(format!("{}/ap/ide", access.url())),
        control_plane: Some("aiplex".into()),
        auth: Some("turn-token".into()),
        audience: Some("https://aiplex.example.com".into()),
        ..ModelBackendConfig::default()
    };
    ModelsConfig {
        backends: [("aiplex-anthropic".to_owned(), backend)].into(),
        routes: vec![ModelRouteConfig {
            model: "claude-*".into(),
            backends: vec!["aiplex-anthropic".into()],
            ..ModelRouteConfig::default()
        }],
        ..ModelsConfig::default()
    }
}

#[test]
fn a_server_serves_the_keys_its_aiplex_token_is_signed_with() {
    let f = Fixture::new();
    let access = MockHttp::start(|request| {
        let model = request.json()["model"].clone();
        Response::json(
            200,
            &json!({"type": "message", "model": model, "content": [],
                "usage": {"input_tokens": 1, "output_tokens": 1}}),
        )
    });
    let mut config = f.config();
    config.models = Some(signed(&access));
    // The listen port is chosen at start: the issuer is the URL given.
    config.triggers.public_url = Some("http://127.0.0.1:7400/".into());
    let server = Server::start(config);
    // Public, no private part, Ed25519.
    let (status, _, body) = raw(server.addr, &get("/.well-known/jwks.json", None));
    assert_eq!(status, 200, "{body}");
    println!("jwks {body}");
    let jwks: Value = serde_json::from_str(&body).unwrap();
    let keys = jwks["keys"].as_array().unwrap();
    assert!(!keys.is_empty());
    for key in keys {
        assert_eq!(key["kty"], "OKP");
        assert_eq!(key["crv"], "Ed25519");
        assert!(key["x"].is_string() && key["kid"].is_string());
        assert!(key.get("alg").is_none() || key["alg"] == "EdDSA");
        assert!(key.get("d").is_none(), "{key}");
    }
    // A turn's call to AIPlex carries a token signed with one of them.
    let script = f.dir.join("harness.py");
    fs::write(&script, HARNESS).unwrap();
    let request = branchyard_client::api::TaskRequest {
        provision: Some(Provisioning {
            models: Some(ModelAccess {
                allow: vec!["claude-*".into()],
            }),
            ..Provisioning::default()
        }),
        ..task(&format!("SH python3 {}", script.display()), "signed")
    };
    let op = run(&server.client(), &request);
    assert!(op.state.is_terminal(), "{op:?}");
    let seen = access.await_requests(1);
    let bearer = seen[0].header("authorization").unwrap();
    let token = bearer.strip_prefix("Bearer ").unwrap();
    let header: Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(token.split('.').next().unwrap())
            .unwrap(),
    )
    .unwrap();
    assert!(
        keys.iter().any(|k| k["kid"] == header["kid"]),
        "{header} {jwks}"
    );
    let claims = branchyard::connectors::keys::verify(&jwks, token).unwrap();
    assert_eq!(claims["iss"], "http://127.0.0.1:7400");
    assert_eq!(claims["aud"], "https://aiplex.example.com");
    assert_eq!(claims["sub"], "branchyard:default");
    assert_eq!(claims["by_branch"], "app/signed");
    assert_eq!(claims["by_purpose"], "aiplex");
    assert!(seen[0].header("x-api-key").is_none());
    access.finish();
}

#[test]
fn a_signed_token_needs_an_issuer_aiplex_can_reach() {
    let f = Fixture::new();
    let access = MockHttp::start(|_| Response::new(200, ""));
    let mut config = f.config();
    config.models = Some(signed(&access));
    // Port 0 is chosen at start: no issuer to name.
    let error = config.validate().unwrap_err();
    assert!(
        error.contains("models.backends.aiplex-anthropic.auth") && error.contains("port 0"),
        "{error}"
    );
    // A fixed loopback port is the issuer.
    config.listen = "127.0.0.1:7401".parse().unwrap();
    config.validate().unwrap();
    assert_eq!(
        branchyard_server::models::issuer(&config),
        "http://127.0.0.1:7401"
    );
    // Plain http elsewhere is not.
    config.triggers.public_url = Some("http://10.0.0.5:7400".into());
    let error = config.validate().unwrap_err();
    assert!(error.contains("not loopback"), "{error}");
    config.triggers.public_url = Some("https://by.example.com".into());
    config.validate().unwrap();
    assert_eq!(
        branchyard_server::models::issuer(&config),
        "https://by.example.com"
    );
    // Without a signed-token backend nothing changes: the issuer is the
    // listen address, public_url or not.
    config.models = None;
    assert_eq!(
        branchyard_server::models::issuer(&config),
        "http://127.0.0.1:7401"
    );
    access.finish();
}
