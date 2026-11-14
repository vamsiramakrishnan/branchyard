//! An AIPlex backend called with a token the yard signs for each turn
//! (`auth = "turn-token"`, contract section 7), through the engine: the
//! token sent upstream, its claims and header, its reuse, a signer that
//! fails, and the gateway refusing it as a turn token. `MockHttp` plays
//! the access point; the signer is the yard's key ring behind a counter.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod aiplex_kit;
mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use aiplex_kit::*;
use base64::Engine as _;
use branchyard::connectors::keys::{self, Claims, KeyRing, TokenSigner};
use branchyard::models::{self, KeySource};
use branchyard::Error;
use common::Fixture;
use serde_json::{json, Value};

const AUDIENCE: &str = "https://aiplex.example.com";
const ISSUER: &str = "http://127.0.0.1:7400";

/// The yard's keys, counting the tokens it signs for AIPlex, and refusing
/// to sign them when `fail` is set.
#[derive(Debug)]
struct Counting {
    ring: KeyRing,
    signed: AtomicUsize,
    fail: bool,
}

impl TokenSigner for Counting {
    fn sign(&self, claims: &Claims) -> Result<String, Error> {
        if claims.by_purpose.as_deref() == Some(models::AIPLEX_PURPOSE) {
            self.signed.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(Error::State("the key service is unavailable".into()));
            }
        }
        self.ring.sign(claims)
    }
    fn jwks(&self) -> Result<Value, Error> {
        self.ring.jwks()
    }
    fn kid(&self) -> Result<String, Error> {
        Ok(self.ring.kids()[0].clone())
    }
}

/// A gateway whose one AIPlex backend at `access` is called with a signed
/// token, issued as a loopback server would.
fn signed_gateway(f: &Fixture, access: &branchyard_testkit::MockHttp, fail: bool) -> Arc<Counting> {
    let config: models::Config = serde_json::from_value(json!({
        "backends": {"aiplex-anthropic": {"api": "anthropic",
            "url": format!("{}/ap/ide", access.url()), "control_plane": "aiplex",
            "auth": "turn-token", "audience": AUDIENCE}},
        "routes": [{"model": "claude-*", "backends": ["aiplex-anthropic"]}],
        "audit": "contents"
    }))
    .unwrap();
    let counting = Arc::new(Counting {
        ring: KeyRing::generate().unwrap(),
        signed: AtomicUsize::new(0),
        fail,
    });
    let mut signer = models::Signer::local(&f.yard).unwrap();
    signer.issuer = ISSUER.into();
    signer.signer = Some(counting.clone());
    let gateway = models::Gateway::new(&config, signer, |n| KeySource::Env(n.into())).unwrap();
    f.yard.use_models(gateway);
    counting
}

fn bearer(request: &branchyard_testkit::Request) -> String {
    let values = request.header_values("authorization");
    assert_eq!(values.len(), 1, "{values:?}");
    values[0].strip_prefix("Bearer ").unwrap().to_owned()
}

fn part(token: &str, n: usize) -> Value {
    let segment = token.split('.').nth(n).unwrap();
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(segment)
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn an_aiplex_backend_is_called_with_a_token_the_yard_signs_for_the_turn() {
    let f = Fixture::new();
    let access = aiplex("one");
    let signer = signed_gateway(&f, &access, false);
    let harness_token = f.dir.join("harness.token");
    let plan = json!([
        call("first", "anthropic", "/v1/messages", "claude-ok-json", false),
        call("second", "anthropic", "/v1/messages", "claude-ok-stream", true),
        {"dump_token": harness_token.display().to_string()},
    ]);
    let (events, read) = run(&f, "signed", None, &plan);
    for label in ["first", "second"] {
        assert_eq!(read[label]["status"], 200, "{label}: {}", read[label]);
    }
    let seen = access.requests();
    assert_eq!(seen.len(), 2);
    // A bearer token, no x-api-key; the same token on both calls, signed
    // once.
    let token = bearer(&seen[0]);
    assert_eq!(bearer(&seen[1]), token);
    for request in &seen {
        assert!(request.header_values("x-api-key").is_empty());
    }
    assert_eq!(signer.signed.load(Ordering::SeqCst), 1);
    // It verifies against the yard's published keys, and says exactly what
    // the contract's table says.
    let jwks = signer.jwks().unwrap();
    let claims = keys::verify(&jwks, &token).unwrap();
    let header = part(&token, 0);
    assert_eq!(header["alg"], "EdDSA");
    assert_eq!(header["typ"], "JWT");
    let kids: Vec<&str> = jwks["keys"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|k| k["kid"].as_str())
        .collect();
    assert!(kids.contains(&header["kid"].as_str().unwrap()), "{header}");
    assert_eq!(claims["iss"], ISSUER);
    assert!(claims["aud"].is_string(), "{claims}");
    assert_eq!(claims["aud"], AUDIENCE);
    assert_eq!(claims["sub"], "branchyard:local");
    assert_eq!(claims["by_tenant"], "local");
    assert_eq!(claims["by_branch"], "signed");
    assert_eq!(claims["by_turn"], "1");
    assert_eq!(claims["by_purpose"], "aiplex");
    let (iat, exp) = (
        claims["iat"].as_u64().unwrap(),
        claims["exp"].as_u64().unwrap(),
    );
    assert!(exp > iat && exp - iat <= 3600, "{iat} {exp}");
    assert!(!claims["jti"].as_str().unwrap().is_empty());
    // None of the turn's scopes.
    assert_eq!(claims["by_grants"], json!([]));
    for scope in ["by_models", "by_network", "by_delegation", "by_manifest"] {
        assert!(claims.get(scope).is_none(), "{scope}: {claims}");
    }
    println!("header {header}\nclaims {claims}");
    // Not the harness's token, which never went upstream.
    let held = std::fs::read_to_string(&harness_token).unwrap();
    assert!(!held.is_empty());
    assert_ne!(held, token);
    for request in &seen {
        assert!(!request.sent_headers.iter().any(|(_, v)| v.contains(&held)));
    }
    // The token is in no event, usage record, audit line or harness output.
    let signature = token.split('.').nth(2).unwrap();
    let recorded = serde_json::to_string(&events).unwrap();
    let rows = serde_json::to_string(&f.yard.model_usage(0).unwrap()).unwrap();
    let audit = read_all(&f.root.join(".branchyard/models/audit"));
    for (what, text) in [("events", &recorded), ("usage", &rows), ("audit", &audit)] {
        assert!(!text.contains(signature), "the token is in the {what}");
    }
    assert!(!read.values().any(|r| r.to_string().contains(signature)));
    // Presented as a turn token, the gateway refuses it.
    let mut presented = call(
        "as a turn token",
        "anthropic",
        "/v1/messages",
        "claude-ok-json",
        false,
    );
    presented["key"] = json!(token);
    let (_, read) = run(&f, "presented", None, &json!([presented]));
    assert_eq!(
        read["as a turn token"]["status"], 401,
        "{}",
        read["as a turn token"]
    );
    assert_eq!(access.requests().len(), 2, "the refused call was forwarded");
    access.finish();
}

#[test]
fn a_signer_that_fails_fails_the_call_closed() {
    let f = Fixture::new();
    let access = aiplex("one");
    let signer = signed_gateway(&f, &access, true);
    let plan = json!([call(
        "call",
        "anthropic",
        "/v1/messages",
        "claude-ok-json",
        false
    )]);
    let (events, read) = run(&f, "unsigned", None, &plan);
    let got = &read["call"];
    assert_eq!(got["status"], 502, "{got}");
    let body = got["body"].as_str().unwrap();
    assert!(body.contains("could not sign the token"), "{body}");
    assert!(
        access.requests().is_empty(),
        "a call went upstream unsigned"
    );
    assert_eq!(signer.signed.load(Ordering::SeqCst), 1);
    let calls: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.activity {
            branchyard::Activity::Model(m) => match m.as_ref() {
                models::ModelActivity::Call(c) => Some(c.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].decision, "failed");
    assert!(calls[0]
        .reason
        .as_deref()
        .unwrap()
        .contains("key service is unavailable"));
    access.finish();
}
