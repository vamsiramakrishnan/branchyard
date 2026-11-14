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
    // With connectors, the issuer is the connector gateway's, which its own
    // tokens are verified against: public_url does not move it, and the
    // message names the setting that does.
    config.models = Some(signed(&access));
    config.connectors = Some(connectors(&f));
    config.listen = "0.0.0.0:8421".parse().unwrap();
    let error = config.validate().unwrap_err();
    assert!(
        error.contains("set connectors.issuer") && !error.contains("public_url"),
        "{error}"
    );
    config.listen = "127.0.0.1:8421".parse().unwrap();
    assert_eq!(
        branchyard_server::models::issuer(&config),
        "http://127.0.0.1:8421"
    );
    if let Some(c) = config.connectors.as_mut() {
        c.issuer = Some("https://by.example.com".into());
    }
    config.validate().unwrap();
    assert_eq!(
        branchyard_server::models::issuer(&config),
        "https://by.example.com"
    );
    access.finish();
}

#[test]
fn a_refused_issuer_names_the_setting_and_the_value_that_work() {
    let f = Fixture::new();
    let access = MockHttp::start(|_| Response::new(200, ""));
    let mut config = f.config();
    config.models = Some(signed(&access));
    let refusal = |config: &branchyard_server::config::Config| config.validate().unwrap_err();
    // TLS on every interface and no public_url: the issuer would be the
    // address the server listens on, which nothing reaches.
    config.tls = Some(branchyard_server::config::TlsFiles {
        cert: f.dir.join("cert.pem"),
        key: f.dir.join("key.pem"),
    });
    for listen in ["0.0.0.0:8443", "[::]:8443"] {
        config.listen = listen.parse().unwrap();
        assert_eq!(
            refusal(&config),
            format!(
                "models.backends.aiplex-anthropic.auth: the issuer \"https://{listen}\" names an \
                 unspecified address, which a server listens on and nothing can be reached at, \
                 so AIPlex cannot fetch keys from it; this issuer comes from listen ({listen}): \
                 set public_url to the URL AIPlex reaches this server at"
            )
        );
    }
    config.triggers.public_url = Some("https://by.example.com".into());
    config.validate().unwrap();
    // Only the spelling of a listen address is wrong: the value to set.
    config.tls = None;
    config.triggers.public_url = None;
    config.listen = "127.0.0.1:80".parse().unwrap();
    assert_eq!(
        refusal(&config),
        "models.backends.aiplex-anthropic.auth: the issuer \"http://127.0.0.1:80\" writes out \
         the default port of http; the issuer must be spelled the same in Branchyard and in \
         AIPlex, so Branchyard takes one spelling (a lower-case host, no trailing dot, no \
         default port): write \"http://127.0.0.1\"; this issuer comes from listen \
         (127.0.0.1:80): set public_url to \"http://127.0.0.1\""
    );
    config.triggers.public_url = Some("http://127.0.0.1".into());
    config.validate().unwrap();
    assert_eq!(
        branchyard_server::models::issuer(&config),
        "http://127.0.0.1"
    );
    // A setting's own value that is wrong is changed where it is.
    config.triggers.public_url = Some("https://By.example.com.".into());
    assert_eq!(
        refusal(&config),
        "models.backends.aiplex-anthropic.auth: the issuer \"https://By.example.com.\" ends \
         its host with a dot and has an upper-case letter in its host; the issuer must be \
         spelled the same in Branchyard and in AIPlex, so Branchyard takes one spelling (a \
         lower-case host, no trailing dot, no default port): write \"https://by.example.com\"; \
         it is public_url's value: change it there"
    );
    config.triggers.public_url = Some("http://10.0.0.5:7400".into());
    let error = refusal(&config);
    assert!(
        error.contains("not loopback")
            && error.ends_with("it is public_url's value: change it there"),
        "{error}"
    );
    // With connectors the setting is the connector gateway's.
    config.triggers.public_url = None;
    config.connectors = Some(connectors(&f));
    config.listen = "0.0.0.0:8421".parse().unwrap();
    let error = refusal(&config);
    assert!(
        error.contains("unspecified address")
            && error.ends_with(
                "this issuer comes from listen (0.0.0.0:8421): set connectors.issuer to the URL \
                 AIPlex reaches this server at"
            ),
        "{error}"
    );
    access.finish();
}

#[test]
fn an_audience_on_another_host_than_the_url_is_a_warning_when_the_file_is_read() {
    let f = Fixture::new();
    let path = f.dir.join("server.json");
    let write = |audience: &str| {
        fs::write(
            &path,
            json!({"listen": "127.0.0.1:7402", "data_dir": "data", "repos": {"app": "repo"},
                "models": {"backends": {"ap": {"api": "anthropic",
                    "url": "https://aiplex.example.com/ap/ide", "control_plane": "aiplex",
                    "auth": "turn-token", "audience": audience}},
                    "routes": [{"model": "*", "backends": ["ap"]}]}})
            .to_string(),
        )
        .unwrap();
    };
    write("https://aiplex.example.com");
    let partial = branchyard_server::config::load_file(&path).unwrap();
    assert!(
        !partial.warnings.iter().any(|w| w.contains("audience")),
        "{:?}",
        partial.warnings
    );
    write("https://elsewhere.example.com");
    let partial = branchyard_server::config::load_file(&path).unwrap();
    assert!(
        partial
            .warnings
            .iter()
            .any(|w| w.contains("models.backends.ap") && w.contains("elsewhere.example.com")),
        "{:?}",
        partial.warnings
    );
}

/// A connector gateway with no issuer of its own.
fn connectors(f: &Fixture) -> branchyard_server::config::ConnectorsConfig {
    branchyard_server::config::ConnectorsConfig {
        gateway: "http://127.0.0.1:9/mcp".into(),
        sandbox_gateway: None,
        issuer: None,
        signing_key: None,
        signing_kms: None,
        audit_file: None,
        bundles: f.dir.join("bundles"),
        anvil: vec!["anvil".into()],
        run_gateway: false,
        listen: None,
        vault_key: None,
        effects_proxy: None,
        effects_listen: None,
        effects_sandbox_host: None,
        sandbox_server: None,
        sandbox_audience: None,
        sandbox_identities: Vec::new(),
        sandbox_jwks_url: None,
        sandbox_proxy_image: None,
        sandbox_by: None,
    }
}
