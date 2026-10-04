//! The web companion over real HTTP on 127.0.0.1: the page's files and
//! their security headers, pairing links (single use, expiry, the rate
//! limit, scopes, revocation), and Web Push to a mock push service that
//! checks the VAPID signature and decrypts the message as a browser would.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use branchyard_server::companion::{self, link, push, store};
use branchyard_testkit::wait;
use common::{get, post, raw, Fixture, Server, TOKEN};
use ring::agreement;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::Value;

fn json(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body:?}"))
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (n, v) = line.split_once(':')?;
        n.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn request(method: &str, path: &str, token: Option<&str>, body: &str) -> String {
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{auth}\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

fn app_config(f: &Fixture) -> branchyard_server::Config {
    let mut config = f.config();
    config.app.enabled = true;
    config
}

/// `branchyard-server token ...` in the fixture's repository, on its data
/// directory: stdout, stderr and whether it succeeded.
fn token_command(f: &Fixture, args: &[&str]) -> (String, String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_branchyard-server"))
        .arg("token")
        .args(args)
        .arg("--data-dir")
        .arg(&f.data)
        .current_dir(&f.root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// The code in a pairing link's fragment.
fn code_of(link: &str) -> String {
    link.trim()
        .split_once("#pair=")
        .map(|(_, code)| code.to_owned())
        .unwrap_or_else(|| panic!("no code in {link:?}"))
}

fn redeem(server: &Server, code: &str) -> (u16, String, String) {
    raw(
        server.addr,
        &post(
            "/app/pair",
            None,
            "",
            &serde_json::json!({ "code": code, "device": "Test browser" }).to_string(),
        ),
    )
}

#[test]
fn the_companion_is_off_unless_configured() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    // Nothing is public: the page's paths need a token like any route,
    // and are unknown with one.
    assert_eq!(raw(server.addr, &get("/app/", None)).0, 401);
    assert_eq!(raw(server.addr, &get("/app/app.js", None)).0, 401);
    assert_eq!(raw(server.addr, &post("/app/pair", None, "", "{}")).0, 401);
    assert_eq!(raw(server.addr, &get("/app/", Some(TOKEN))).0, 404);
    assert_eq!(raw(server.addr, &get("/v1/app/me", Some(TOKEN))).0, 404);
    assert_eq!(raw(server.addr, &get("/v1/app/push", Some(TOKEN))).0, 404);
}

#[test]
fn the_page_is_served_with_a_strict_policy() {
    let f = Fixture::new();
    let server = Server::start(app_config(&f));
    let (status, head, _) = raw(server.addr, &get("/app", None));
    assert_eq!(status, 308);
    assert_eq!(header(&head, "location"), Some("/app/"));

    let (status, head, body) = raw(server.addr, &get("/app/", None));
    assert_eq!(status, 200, "{head}");
    assert_eq!(
        header(&head, "content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        header(&head, "content-security-policy"),
        Some(companion::CSP)
    );
    assert_eq!(header(&head, "x-content-type-options"), Some("nosniff"));
    assert_eq!(header(&head, "x-frame-options"), Some("DENY"));
    assert_eq!(header(&head, "referrer-policy"), Some("no-referrer"));
    assert_eq!(header(&head, "cache-control"), Some("no-cache"));
    assert!(body.contains(r#"<script src="app.js" defer></script>"#));
    let etag = header(&head, "etag").unwrap().to_owned();
    let revalidate = format!(
        "GET /app/ HTTP/1.1\r\nHost: x\r\nConnection: close\r\nIf-None-Match: {etag}\r\n\r\n"
    );
    assert_eq!(raw(server.addr, &revalidate).0, 304);

    for (path, kind) in [
        ("/app/app.js", "text/javascript; charset=utf-8"),
        ("/app/app.css", "text/css; charset=utf-8"),
        ("/app/sw.js", "text/javascript; charset=utf-8"),
        ("/app/manifest.webmanifest", "application/manifest+json"),
        ("/app/icon.svg", "image/svg+xml"),
    ] {
        let (status, head, _) = raw(server.addr, &get(path, None));
        assert_eq!(status, 200, "{path}");
        assert_eq!(header(&head, "content-type"), Some(kind), "{path}");
        assert_eq!(
            header(&head, "content-security-policy"),
            Some(companion::CSP)
        );
    }
    assert_eq!(raw(server.addr, &get("/app/../v1/repos", None)).0, 401);
    assert_eq!(raw(server.addr, &get("/app/secret.txt", None)).0, 401);
    assert_eq!(
        raw(server.addr, &get("/app/secret.txt", Some(TOKEN))).0,
        404
    );
    // The API itself still needs a token, and a wrong one is refused.
    assert_eq!(raw(server.addr, &get("/v1/repos", None)).0, 401);
    assert_eq!(
        raw(
            server.addr,
            &get("/v1/repos", Some("not-a-paired-token-000"))
        )
        .0,
        401
    );
    // A configured credential reads who it is.
    let (status, _, body) = raw(server.addr, &get("/v1/app/me", Some(TOKEN)));
    assert_eq!(status, 200, "{body}");
    let me = json(&body);
    assert_eq!(me["name"], "tester");
    assert_eq!(me["kind"], "configured");
    assert!(me.get("expires_at_ms").is_none());
}

#[test]
fn a_pairing_link_gives_a_scoped_token_once_until_revoked() {
    let f = Fixture::new();
    let server = Server::start(app_config(&f));
    let (out, err, ok) = token_command(
        &f,
        &[
            "new", "--link", "--name", "phone", "--scopes", "read", "--ttl", "1h",
        ],
    );
    assert!(ok, "{err}");
    assert!(
        out.trim().starts_with("http://127.0.0.1:8421/app/#pair="),
        "{out}"
    );
    assert!(err.contains("pairing link for phone"), "{err}");
    assert!(!err.contains('\u{1b}'), "no QR code off a terminal: {err}");
    let code = code_of(&out);

    let started = branchyard_server::ops::now_ms();
    let (status, head, body) = redeem(&server, &code);
    assert_eq!(status, 200, "{body}");
    assert_eq!(header(&head, "cache-control"), Some("no-store"));
    let paired = json(&body);
    let token = paired["token"].as_str().unwrap().to_owned();
    assert_eq!(paired["me"]["name"], "phone");
    assert_eq!(paired["me"]["kind"], "paired");
    assert_eq!(paired["me"]["scopes"], serde_json::json!(["read"]));
    let expires = paired["me"]["expires_at_ms"].as_u64().unwrap();
    assert!(expires >= started + 3_590_000 && expires <= started + 3_610_000 + 60_000);

    // Single use.
    let (status, _, body) = redeem(&server, &code);
    assert_eq!(status, 400);
    assert_eq!(json(&body)["error"]["code"], "invalid_pairing_code");

    // The token reads, and is refused what its scopes do not allow.
    assert_eq!(raw(server.addr, &get("/v1/repos", Some(&token))).0, 200);
    let (status, _, body) = raw(server.addr, &get("/v1/app/me", Some(&token)));
    assert_eq!(status, 200);
    assert_eq!(json(&body)["expires_at_ms"].as_u64(), Some(expires));
    let (status, _, body) = raw(
        server.addr,
        &post(
            "/v1/repos/app/tasks",
            Some(&token),
            "",
            r#"{"prompt": "WRITE x.txt=1", "harness": "gemini-cli"}"#,
        ),
    );
    assert_eq!(status, 403, "{body}");
    assert_eq!(json(&body)["error"]["code"], "scope_required");
    assert_eq!(json(&body)["error"]["detail"]["scope"], "run");

    // Only the hash is kept.
    let (out, err, ok) = token_command(&f, &["list"]);
    assert!(ok, "{err}");
    assert!(
        out.contains("phone") && out.contains("active") && out.contains("Test browser"),
        "{out}"
    );
    assert!(!out.contains(&token));
    for file in ["state.db", "state.db-wal"] {
        let bytes = std::fs::read(f.data.join(file)).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(&token) && !text.contains(&code), "{file}");
    }

    // Pairing flags need --link.
    let (_, err, ok) = token_command(&f, &["new", "--ttl", "1h"]);
    assert!(!ok && err.contains("go with --link"), "{err}");

    // A second link of the same name is refused while the token is live.
    let (_, err, ok) = token_command(&f, &["new", "--link", "--name", "phone"]);
    assert!(!ok && err.contains("already names"), "{err}");

    let (_, err, ok) = token_command(&f, &["revoke", "phone"]);
    assert!(ok, "{err}");
    assert_eq!(raw(server.addr, &get("/v1/repos", Some(&token))).0, 401);
    let (_, err, ok) = token_command(&f, &["revoke", "phone"]);
    assert!(!ok && err.contains("no active paired token"), "{err}");
    let (_, err, ok) = token_command(&f, &["revoke", "default"]);
    assert!(!ok && err.contains("configuration"), "{err}");
    let (out, _, _) = token_command(&f, &["list"]);
    assert!(out.contains("revoked"), "{out}");
}

#[test]
fn expired_codes_and_tokens_are_refused() {
    let f = Fixture::new();
    let config = app_config(&f);
    let server = Server::start(config.clone());
    let companion = store::open(&config).unwrap();
    let now = branchyard_server::ops::now_ms();
    let request = |name: &str, ttl: Duration| link::LinkRequest {
        name: Some(name.into()),
        tenant: "default".into(),
        scopes: vec!["read".into()],
        repos: None,
        ttl: Some(ttl),
        code_ttl: None,
        public_url: None,
        qr: false,
    };
    // A code past its expiry.
    let (_, code) = link::create(
        &config,
        companion.as_ref(),
        &request("late", Duration::from_secs(60)),
        now - 11 * 60 * 1000,
    )
    .unwrap();
    assert_eq!(redeem(&server, &code).0, 400);

    // A token that lasts two seconds stops working after them.
    let (_, code) = link::create(
        &config,
        companion.as_ref(),
        &request("brief", Duration::from_secs(2)),
        now,
    )
    .unwrap();
    let (status, _, body) = redeem(&server, &code);
    assert_eq!(status, 200, "{body}");
    let token = json(&body)["token"].as_str().unwrap().to_owned();
    assert_eq!(raw(server.addr, &get("/v1/repos", Some(&token))).0, 200);
    wait::until("the token to expire", || {
        raw(server.addr, &get("/v1/repos", Some(&token))).0 == 401
    });

    // Codes are case-insensitive hex; junk is refused.
    for junk in ["", "zz", &"a".repeat(200)] {
        assert_eq!(redeem(&server, junk).0, 400, "{junk}");
    }
}

#[test]
fn pairing_attempts_are_rate_limited() {
    let f = Fixture::new();
    let config = app_config(&f);
    let server = Server::start(config.clone());
    let companion = store::open(&config).unwrap();
    for i in 0..companion::PAIR_ATTEMPTS {
        assert_eq!(redeem(&server, &format!("{i:032x}")).0, 400);
    }
    let request = link::LinkRequest {
        name: Some("phone".into()),
        tenant: "default".into(),
        scopes: vec!["read".into()],
        repos: None,
        ttl: None,
        code_ttl: None,
        public_url: None,
        qr: false,
    };
    let (_, code) = link::create(
        &config,
        companion.as_ref(),
        &request,
        branchyard_server::ops::now_ms(),
    )
    .unwrap();
    // Even a good code waits once the limit is reached; it stays unused.
    let (status, head, body) = redeem(&server, &code);
    assert_eq!(status, 429, "{body}");
    assert_eq!(json(&body)["error"]["code"], "rate_limited");
    let retry: u64 = header(&head, "retry-after").unwrap().parse().unwrap();
    assert!((1..=60).contains(&retry));
    let (_, codes) = companion.tokens().unwrap();
    assert_eq!(codes.len(), 1);
}

#[test]
fn a_paired_event_stream_ends_when_its_token_is_revoked() {
    let f = Fixture::new();
    let config = app_config(&f);
    let server = Server::start(config.clone());
    let companion = store::open(&config).unwrap();
    let request = link::LinkRequest {
        name: Some("watcher".into()),
        tenant: "default".into(),
        scopes: vec!["read".into()],
        repos: None,
        ttl: None,
        code_ttl: None,
        public_url: None,
        qr: false,
    };
    let (_, code) = link::create(
        &config,
        companion.as_ref(),
        &request,
        branchyard_server::ops::now_ms(),
    )
    .unwrap();
    let token = json(&redeem(&server, &code).2)["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut stream = TcpStream::connect(server.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    stream
        .write_all(get("/v1/repos/app/events/stream", Some(&token)).as_bytes())
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 200"), "{line}");
    let started = Instant::now();
    companion
        .revoke("watcher", None, branchyard_server::ops::now_ms())
        .unwrap();
    let mut rest = Vec::new();
    // The stream ends (rather than hanging until the read timeout).
    let _ = reader.read_to_end(&mut rest);
    assert!(
        started.elapsed() < Duration::from_secs(45),
        "the stream outlived its revoked token"
    );
}

/// One delivery to the mock push service: path, head, body.
type Delivery = (String, String, Vec<u8>);

/// A push service on 127.0.0.1 that records each delivery and answers 201,
/// or 410 for an endpoint ending in `/gone`.
struct MockPush {
    addr: SocketAddr,
    received: Arc<Mutex<Vec<Delivery>>>,
}

impl MockPush {
    fn start() -> MockPush {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = received.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let log = log.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream);
                    let mut head = String::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                        head.push_str(&line);
                    }
                    let length: usize = header(&head, "content-length")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    let mut body = vec![0u8; length];
                    reader.read_exact(&mut body).unwrap();
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                    let gone = path.ends_with("/gone");
                    log.lock().unwrap().push((path, head, body));
                    let answer = match gone {
                        true => {
                            "HTTP/1.1 410 Gone\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        }
                        false => {
                            "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        }
                    };
                    let _ = reader.get_mut().write_all(answer.as_bytes());
                });
            }
        });
        MockPush { addr, received }
    }

    fn count(&self) -> usize {
        self.received.lock().unwrap().len()
    }
}

/// A browser's subscription keys: its private key (good for one
/// decryption), public key and authentication secret.
fn browser_keys() -> (agreement::EphemeralPrivateKey, Vec<u8>, Vec<u8>) {
    let rng = SystemRandom::new();
    let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
    let public = private.compute_public_key().unwrap().as_ref().to_vec();
    let mut auth = vec![0u8; 16];
    rng.fill(&mut auth).unwrap();
    (private, public, auth)
}

fn subscribe(
    server: &Server,
    token: &str,
    endpoint: &str,
    public: &[u8],
    auth: &[u8],
) -> (u16, String) {
    let body = serde_json::json!({
        "endpoint": endpoint,
        "expirationTime": null,
        "keys": { "p256dh": URL_SAFE_NO_PAD.encode(public), "auth": URL_SAFE_NO_PAD.encode(auth) },
    })
    .to_string();
    let (status, _, body) = raw(
        server.addr,
        &request("POST", "/v1/app/push/subscriptions", Some(token), &body),
    );
    (status, body)
}

#[test]
fn push_notifications_reach_a_subscribed_browser() {
    let mock = MockPush::start();
    let f = Fixture::new();
    let mut config = app_config(&f);
    config.app.push_services = vec!["127.0.0.1".into()];
    config.app.push_subject = Some("mailto:ops@example.com".into());
    let server = Server::start(config);

    let (status, _, body) = raw(server.addr, &get("/v1/app/push", Some(TOKEN)));
    assert_eq!(status, 200, "{body}");
    let info = json(&body);
    assert_eq!(info["enabled"], true);
    let public_key = info["public_key"].as_str().unwrap().to_owned();
    assert_eq!(push::decode_b64(&public_key).unwrap().len(), 65);
    assert!(f.data.join("companion/vapid.pk8").is_file());

    // Only listed push services, and well-formed keys.
    let (_, public, auth) = browser_keys();
    let (status, body) = subscribe(&server, TOKEN, "https://evil.example/push", &public, &auth);
    assert_eq!(status, 400, "{body}");
    let (status, body) = subscribe(
        &server,
        TOKEN,
        &format!("http://{}/push/x", mock.addr),
        &public[..10],
        &auth,
    );
    assert_eq!(status, 400, "{body}");

    let (private, public, auth) = browser_keys();
    let endpoint = format!("http://{}/push/one", mock.addr);
    let (status, body) = subscribe(&server, TOKEN, &endpoint, &public, &auth);
    assert_eq!(status, 200, "{body}");
    assert_eq!(json(&body)["subscriptions"], 1);

    // A permission request is news: the default policy denies it, and the
    // browser is told.
    let task = branchyard_client::api::TaskRequest {
        prompt: "PERMISSION WRITE x.txt=1".into(),
        harness: Some("gemini-cli".into()),
        name: Some("asks".into()),
        ..Default::default()
    };
    let op = common::run(&server.client(), &task);
    assert_eq!(
        op.state,
        branchyard_client::api::OperationState::Succeeded,
        "{op:?}"
    );
    wait::until("a push for the permission request", || mock.count() >= 1);
    let (path, head, body) = mock.received.lock().unwrap()[0].clone();
    assert_eq!(path, "/push/one");
    assert_eq!(header(&head, "content-encoding"), Some("aes128gcm"));
    assert_eq!(header(&head, "ttl"), Some("86400"));
    assert_eq!(header(&head, "urgency"), Some("high"));
    let authorization = header(&head, "authorization").unwrap();
    let claims = push::verify_vapid(authorization).unwrap();
    assert_eq!(claims["aud"], format!("http://{}", mock.addr));
    assert_eq!(claims["sub"], "mailto:ops@example.com");
    assert!(authorization.ends_with(&format!("k={public_key}")));
    let plain = push::decrypt_with(&body, private, &public, &auth).unwrap();
    let notice: Value = serde_json::from_slice(&plain).unwrap();
    assert_eq!(notice["kind"], "permission");
    assert_eq!(notice["repo"], "app");
    assert_eq!(notice["branch"], "asks");
    assert_eq!(notice["url"], "#/b/app/asks");
    assert!(notice["body"].as_str().unwrap().contains("asks to use"));
    // Then the finished turn.
    wait::until("a push for the finished turn", || mock.count() >= 2);

    // A push service that says the subscription is gone loses it.
    let (_, public2, auth2) = browser_keys();
    let gone = format!("http://{}/push/gone", mock.addr);
    assert_eq!(subscribe(&server, TOKEN, &gone, &public2, &auth2).0, 200);
    let before = mock.count();
    let (status, _, body) = raw(server.addr, &request_test(TOKEN));
    assert_eq!(status, 200, "{body}");
    let result = json(&body);
    assert_eq!(result["delivered"], 1, "{result}");
    assert_eq!(
        result["subscriptions"], 1,
        "the gone one was dropped: {result}"
    );
    assert_eq!(mock.count(), before + 2);

    // Unsubscribing is the caller's own.
    let (status, _, body) = raw(
        server.addr,
        &request(
            "DELETE",
            "/v1/app/push/subscriptions",
            Some(TOKEN),
            &serde_json::json!({ "endpoint": endpoint }).to_string(),
        ),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(json(&body)["subscriptions"], 0);
}

fn request_test(token: &str) -> String {
    request("POST", "/v1/app/push/test", Some(token), "{}")
}

#[test]
fn a_subscription_follows_its_tokens_reach() {
    let mock = MockPush::start();
    let f = Fixture::new();
    let mut config = app_config(&f);
    config.app.push_services = vec!["127.0.0.1".into()];
    let server = Server::start(config.clone());
    let companion = store::open(&config).unwrap();
    // A paired token without read cannot subscribe.
    let write_only = link::LinkRequest {
        name: Some("blind".into()),
        tenant: "default".into(),
        scopes: vec!["run".into()],
        repos: None,
        ttl: None,
        code_ttl: None,
        public_url: None,
        qr: false,
    };
    let (_, code) = link::create(
        &config,
        companion.as_ref(),
        &write_only,
        branchyard_server::ops::now_ms(),
    )
    .unwrap();
    let blind = json(&redeem(&server, &code).2)["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let (_, public, auth) = browser_keys();
    let endpoint = format!("http://{}/push/blind", mock.addr);
    let (status, body) = subscribe(&server, &blind, &endpoint, &public, &auth);
    assert_eq!(status, 403, "{body}");

    // A paired reader's subscription stops with its token.
    let reader = link::LinkRequest {
        name: Some("reader".into()),
        scopes: vec!["read".into()],
        ..write_only
    };
    let (_, code) = link::create(
        &config,
        companion.as_ref(),
        &reader,
        branchyard_server::ops::now_ms(),
    )
    .unwrap();
    let token = json(&redeem(&server, &code).2)["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let endpoint = format!("http://{}/push/reader", mock.addr);
    assert_eq!(subscribe(&server, &token, &endpoint, &public, &auth).0, 200);
    let (status, _, body) = raw(server.addr, &request_test(&token));
    assert_eq!(status, 200, "{body}");
    assert_eq!(json(&body)["delivered"], 1);
    companion
        .revoke("reader", None, branchyard_server::ops::now_ms())
        .unwrap();
    assert!(companion.subscriptions().unwrap().is_empty());
    assert_eq!(raw(server.addr, &request_test(&token)).0, 401);
}

#[test]
fn the_configuration_file_turns_the_companion_on() {
    let f = Fixture::new();
    let path = f.dir.join("server.json");
    let write = |app: Value| {
        std::fs::write(
            &path,
            serde_json::json!({
                "data_dir": f.data,
                "repos": { "app": f.root },
                "tokens": [{ "name": "t", "token": TOKEN }],
                "app": app,
            })
            .to_string(),
        )
        .unwrap();
        branchyard_server::cli::resolve(&["--config".into(), path.display().to_string()])
    };
    assert!(write(serde_json::json!(true)).unwrap().app.enabled);
    assert!(!write(serde_json::json!(false)).unwrap().app.enabled);
    let config = write(serde_json::json!({
        "push": false,
        "push_subject": "mailto:ops@example.com",
        "push_services": ["push.example.com"],
        "vapid_key": "keys/vapid.pk8"
    }))
    .unwrap();
    assert!(config.app.enabled && !config.app.push);
    assert_eq!(
        config.app.push_services,
        vec!["push.example.com".to_owned()]
    );
    assert_eq!(config.app.vapid_key, Some(f.dir.join("keys/vapid.pk8")));
    let bad = write(serde_json::json!({ "push_subject": "ops@example.com" })).unwrap();
    assert!(bad.validate().unwrap_err().contains("push_subject"));
    assert!(write(serde_json::json!({ "pushes": true })).is_err());
    // The flag turns it on too.
    let flagged = branchyard_server::cli::resolve(&[
        "--repo".into(),
        format!("app={}", f.root.display()),
        "--data-dir".into(),
        f.data.display().to_string(),
        "--app".into(),
    ])
    .unwrap();
    assert!(flagged.app.enabled && flagged.app.push);
}

#[cfg(feature = "postgres")]
#[test]
fn the_postgres_store_conforms() {
    use store::CompanionStore;
    let Some(base) = std::env::var("BY_TEST_POSTGRES_URL")
        .ok()
        .filter(|u| !u.is_empty())
    else {
        eprintln!("skipped: set BY_TEST_POSTGRES_URL to run the PostgreSQL companion tests");
        return;
    };
    let schema = format!("companion_{}", std::process::id());
    let mut client = postgres::Client::connect(&base, postgres::NoTls).unwrap();
    client
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}"
        ))
        .unwrap();
    let separator = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{separator}options=-csearch_path%3D{schema}");
    // Two opens race to make the tables, as two servers starting would.
    let racers: Vec<_> = (0..2)
        .map(|_| {
            let url = url.clone();
            std::thread::spawn(move || store::PostgresCompanion::open(&url).unwrap())
        })
        .collect();
    let stores: Vec<_> = racers.into_iter().map(|r| r.join().unwrap()).collect();
    store::conformance::all(&stores[0]);
    // A code made through one is redeemed once across both.
    let pairing = store::Pairing {
        code_sha256: "shared".into(),
        principal: store::conformance::principal("both", "default"),
        token_ttl_ms: 1000,
        created_at_ms: 0,
        expires_at_ms: 100,
    };
    assert!(stores[0].create_pairing(&pairing, 0).unwrap());
    let a = std::thread::scope(|s| {
        let x = s.spawn(|| stores[0].redeem("shared", "ta", None, 1).unwrap());
        let y = s.spawn(|| stores[1].redeem("shared", "tb", None, 1).unwrap());
        [x.join().unwrap(), y.join().unwrap()]
    });
    assert_eq!(a.iter().filter(|r| r.is_some()).count(), 1);
    drop(stores);
    client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .unwrap();
}
