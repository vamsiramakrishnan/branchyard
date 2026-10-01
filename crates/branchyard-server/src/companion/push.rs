//! Web Push to the companion page (`docs/companion.md#notifications`):
//! VAPID (RFC 8292) request signing, RFC 8291 `aes128gcm` message
//! encryption, which feed entries are worth a notification (the same ones
//! `by watch` notifies about), and the task that sends them.
//!
//! All cryptography is `ring`'s, already in the build for TLS: ECDSA P-256
//! for the VAPID JWT, ephemeral ECDH P-256, HKDF-SHA-256 and AES-128-GCM.
//! A push service only ever sees the encrypted payload.
//!
//! Delivery follows each served repository's durable feed by its own
//! cursor (the webhook cursors' table, keyed `<repo>:companion-push`), so
//! a restart resumes instead of replaying; entries older than
//! [`FRESH_MS`] when first read (after downtime) are skipped rather than
//! sent late. One attempt per notification: a push is a nudge, and the
//! page shows the same state when opened. A subscription whose push
//! service answers 404 or 410, or whose credential no longer verifies
//! (revoked, expired, removed from the configuration), is dropped.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use branchyard::{Activity, BranchStatus, Event, MessageKind};
use branchyard_client::api::FeedEntry;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use ring::{aead, agreement, hkdf};
use serde::Serialize;
use tokio::sync::watch;

use super::store::Subscription;
use crate::api::{App, RepoState};
use crate::config::Principal;
use crate::store::OperationStore;

/// What a notification can be about, as subscriptions name them.
pub const KINDS: &[&str] = &[
    "permission",
    "question",
    "stalled",
    "failed",
    "interrupted",
    "finished",
];

/// Entries older than this when first read are history, not news.
pub const FRESH_MS: u64 = 10 * 60 * 1000;
/// The record size every message declares; one record holds a payload.
const RECORD_SIZE: u32 = 4096;
/// The largest payload sent, well inside one record.
const MAX_PAYLOAD: usize = 3000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const BATCH: usize = 100;
const POLL: Duration = Duration::from_secs(5);
/// How long a push service keeps an undelivered message.
const TTL_SECONDS: u32 = 24 * 60 * 60;

pub fn check_kind(kind: &str) -> Result<(), String> {
    match KINDS.contains(&kind) {
        true => Ok(()),
        false => Err(format!(
            "{kind:?} is not a notification kind; use {}",
            KINDS.join(", ")
        )),
    }
}

// ---------------------------------------------------------------------
// Which activity is worth a notification

/// One notification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Notice {
    pub kind: &'static str,
    pub repo: String,
    pub branch: String,
    pub title: String,
    pub body: String,
    /// Where the page opens on a tap, relative to `/app/`.
    pub url: String,
    /// Replaces an earlier notification with the same tag.
    pub tag: String,
}

fn short(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.chars().count() > max {
        true => format!("{}…", line.chars().take(max - 1).collect::<String>()),
        false => line,
    }
}

/// Decides which feed entries deserve a notice, each once: the
/// server-side counterpart of `by watch`'s tracker (`branchyard-cli`'s
/// `notify::Tracker`), with the same kinds.
#[derive(Default)]
pub struct Tracker {
    seen: HashSet<String>,
    order: VecDeque<String>,
    status: HashMap<String, &'static str>,
}

impl Tracker {
    fn first(&mut self, key: String) -> bool {
        if self.seen.contains(&key) {
            return false;
        }
        self.seen.insert(key.clone());
        self.order.push_back(key);
        while self.order.len() > 4096 {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }

    pub fn observe(&mut self, repo: &str, branch: &str, activity: &Activity) -> Option<Notice> {
        let (kind, text): (&'static str, String) = match activity {
            Activity::Harness(Event::PermissionRequested { request, .. }) => {
                if !self.first(format!("permission:{branch}:{}", request.key.0)) {
                    return None;
                }
                (
                    "permission",
                    format!(
                        "{branch} asks to use {}; the request's policy decides",
                        request.tool
                    ),
                )
            }
            Activity::Message(message)
                if matches!(
                    message.kind,
                    MessageKind::Question | MessageKind::Escalation
                ) =>
            {
                if !self.first(format!("message:{}", message.id)) {
                    return None;
                }
                let verb = match message.kind {
                    MessageKind::Question => "asks",
                    _ => "escalates to",
                };
                (
                    "question",
                    format!("{} {verb} {}: {}", message.from, message.to, message.text),
                )
            }
            Activity::Stalled { since_ms } => {
                if !self.first(format!("stall:{branch}:{since_ms}")) {
                    return None;
                }
                (
                    "stalled",
                    format!("{branch} has stalled: no activity from its harness"),
                )
            }
            Activity::Status(status) => {
                let (kind, text) = match status {
                    BranchStatus::Running | BranchStatus::Waiting => {
                        self.status.remove(branch);
                        return None;
                    }
                    BranchStatus::Merged { .. } => return None,
                    BranchStatus::Ready => ("finished", format!("{branch} is ready to merge")),
                    BranchStatus::NoChanges => {
                        ("finished", format!("{branch} finished with no changes"))
                    }
                    BranchStatus::BudgetExceeded { .. } => {
                        ("finished", format!("{branch} stopped at its budget"))
                    }
                    BranchStatus::Interrupted => {
                        ("interrupted", format!("{branch} was interrupted"))
                    }
                    BranchStatus::Failed { reason } => {
                        ("failed", format!("{branch} failed: {reason}"))
                    }
                    BranchStatus::Blocked { reason } => {
                        ("failed", format!("{branch} is blocked: {reason}"))
                    }
                    BranchStatus::AwaitingPlanApproval => (
                        "question",
                        format!("{branch} has a plan waiting for your approval"),
                    ),
                };
                if self.status.insert(branch.to_owned(), kind) == Some(kind) {
                    return None;
                }
                (kind, text)
            }
            _ => return None,
        };
        let target = match activity {
            Activity::Message(message) => message.to.clone(),
            _ => branch.to_owned(),
        };
        Some(Notice {
            kind,
            repo: repo.to_owned(),
            branch: target.clone(),
            title: format!("{repo}: {target}"),
            body: short(&text, 240),
            url: match kind {
                "question" => "#/inbox".to_owned(),
                _ => format!("#/b/{}/{}", url_part(repo), url_part(&target)),
            },
            tag: format!("{repo}/{target}/{kind}"),
        })
    }
}

/// `text` percent-encoded for a URL fragment segment.
fn url_part(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------
// VAPID

/// The server's VAPID key pair (an ECDSA P-256 key, PKCS#8 on disk).
pub struct Vapid {
    key: EcdsaKeyPair,
    subject: String,
    rng: SystemRandom,
}

impl Vapid {
    /// The key at `path`, made (mode 600) when missing.
    pub fn load_or_create(path: &std::path::Path, subject: String) -> Result<Vapid, String> {
        let rng = SystemRandom::new();
        let pkcs8 = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let document = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
                    .map_err(|_| "could not generate a VAPID key".to_owned())?;
                write_private(path, document.as_ref())?;
                document.as_ref().to_vec()
            }
            Err(e) => return Err(format!("VAPID key {}: {e}", path.display())),
        };
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &rng)
            .map_err(|e| format!("VAPID key {}: {e}", path.display()))?;
        Ok(Vapid { key, subject, rng })
    }

    /// A fresh key, kept in memory only: for tests.
    pub fn ephemeral(subject: &str) -> Vapid {
        let rng = SystemRandom::new();
        let document = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .expect("a P-256 key");
        let key =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, document.as_ref(), &rng)
                .expect("its own key");
        Vapid {
            key,
            subject: subject.to_owned(),
            rng,
        }
    }

    /// The uncompressed public point, base64url: the browser's
    /// `applicationServerKey` and the `k` of every request.
    pub fn public_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.key.public_key().as_ref())
    }

    /// `Authorization: vapid t=JWT, k=KEY` for a push to `endpoint`'s
    /// origin, valid for 12 hours from `now_s`.
    pub fn authorization(&self, endpoint: &reqwest::Url, now_s: u64) -> Result<String, String> {
        let audience = endpoint.origin().ascii_serialization();
        let header = URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = serde_json::json!({
            "aud": audience,
            "exp": now_s + 12 * 60 * 60,
            "sub": self.subject,
        });
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes());
        let input = format!("{header}.{claims}");
        let signature = self
            .key
            .sign(&self.rng, input.as_bytes())
            .map_err(|_| "could not sign the VAPID token".to_owned())?;
        Ok(format!(
            "vapid t={input}.{}, k={}",
            URL_SAFE_NO_PAD.encode(signature.as_ref()),
            self.public_key()
        ))
    }
}

/// Write `bytes` to `path`, readable only by this user.
fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("VAPID key {}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("VAPID key {}: {e}", path.display()))
}

// ---------------------------------------------------------------------
// RFC 8291 message encryption

struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn hkdf_expand(prk: &hkdf::Prk, info: &[&[u8]], len: usize) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; len];
    prk.expand(info, Len(len))
        .and_then(|okm| okm.fill(&mut out))
        .map_err(|_| "HKDF expansion failed".to_owned())?;
    Ok(out)
}

/// Decode base64url with or without padding (or the standard alphabet).
pub fn decode_b64(text: &str) -> Result<Vec<u8>, String> {
    let normalized: String = text
        .trim()
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    URL_SAFE_NO_PAD
        .decode(normalized)
        .map_err(|e| format!("not base64url: {e}"))
}

/// The keys a subscription's message is derived from (RFC 8291 §3.3,
/// §3.4): the content encryption key and nonce, from the shared ECDH
/// secret, the subscription's authentication secret, both public keys and
/// the message's salt.
fn derive(
    ecdh_secret: &[u8],
    auth_secret: &[u8],
    ua_public: &[u8],
    as_public: &[u8],
    salt: &[u8],
) -> Result<([u8; 16], [u8; 12]), String> {
    let prk_key = hkdf::Salt::new(hkdf::HKDF_SHA256, auth_secret).extract(ecdh_secret);
    let ikm = hkdf_expand(&prk_key, &[b"WebPush: info\0", ua_public, as_public], 32)?;
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(&ikm);
    let cek = hkdf_expand(&prk, &[b"Content-Encoding: aes128gcm\0"], 16)?;
    let nonce = hkdf_expand(&prk, &[b"Content-Encoding: nonce\0"], 12)?;
    Ok((
        cek.try_into().expect("16 bytes"),
        nonce.try_into().expect("12 bytes"),
    ))
}

/// `payload` encrypted for the browser whose public key is `p256dh` and
/// authentication secret `auth` (both base64url): the `aes128gcm` body of
/// RFC 8188, one record, with a fresh ephemeral key and salt.
pub fn encrypt(payload: &[u8], p256dh: &str, auth: &str) -> Result<Vec<u8>, String> {
    let ua_public = decode_b64(p256dh).map_err(|e| format!("p256dh: {e}"))?;
    let auth_secret = decode_b64(auth).map_err(|e| format!("auth: {e}"))?;
    if ua_public.len() != 65 || ua_public[0] != 4 {
        return Err("p256dh is not an uncompressed P-256 point".into());
    }
    if auth_secret.len() != 16 {
        return Err("auth is not 16 bytes".into());
    }
    if payload.len() > MAX_PAYLOAD {
        return Err("payload too large".into());
    }
    let rng = SystemRandom::new();
    let ephemeral = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
        .map_err(|_| "could not make an ephemeral key".to_owned())?;
    let as_public = ephemeral
        .compute_public_key()
        .map_err(|_| "could not compute the ephemeral public key".to_owned())?;
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, &ua_public);
    let ecdh_secret = agreement::agree_ephemeral(ephemeral, &peer, |secret| secret.to_vec())
        .map_err(|_| "the subscription's p256dh key was refused".to_owned())?;
    let mut salt = [0u8; 16];
    rng.fill(&mut salt)
        .map_err(|_| "no randomness for the salt".to_owned())?;
    let (cek, nonce) = derive(
        &ecdh_secret,
        &auth_secret,
        &ua_public,
        as_public.as_ref(),
        &salt,
    )?;
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_128_GCM, &cek).map_err(|_| "bad key".to_owned())?,
    );
    let mut record = payload.to_vec();
    record.push(0x02); // the last record's delimiter, no padding
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        aead::Aad::empty(),
        &mut record,
    )
    .map_err(|_| "encryption failed".to_owned())?;
    let mut body = Vec::with_capacity(16 + 4 + 1 + 65 + record.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(as_public.as_ref().len() as u8);
    body.extend_from_slice(as_public.as_ref());
    body.extend_from_slice(&record);
    Ok(body)
}

/// The receiving side of [`encrypt`], as a browser does it, given the
/// ECDH secret it computed with its private key: for tests and the mock
/// push service in `tests/companion.rs`, since `ring` keeps a private
/// key's agreement to one use.
#[doc(hidden)]
pub fn decrypt_with(
    body: &[u8],
    ua_private: agreement::EphemeralPrivateKey,
    ua_public: &[u8],
    auth_secret: &[u8],
) -> Result<Vec<u8>, String> {
    if body.len() < 21 {
        return Err("too short".into());
    }
    let salt = &body[..16];
    let id_len = body[20] as usize;
    let as_public = body
        .get(21..21 + id_len)
        .ok_or_else(|| "truncated key id".to_owned())?;
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public);
    let ecdh_secret = agreement::agree_ephemeral(ua_private, &peer, |s| s.to_vec())
        .map_err(|_| "bad key id".to_owned())?;
    let (cek, nonce) = derive(&ecdh_secret, auth_secret, ua_public, as_public, salt)?;
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_128_GCM, &cek).map_err(|_| "bad key".to_owned())?,
    );
    let mut record = body[21 + id_len..].to_vec();
    let plain = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::empty(),
            &mut record,
        )
        .map_err(|_| "authentication failed".to_owned())?;
    match plain.iter().rposition(|&b| b != 0) {
        Some(at) if plain[at] == 0x02 => Ok(plain[..at].to_vec()),
        _ => Err("no last-record delimiter".into()),
    }
}

/// Verify a VAPID `Authorization` header's token against its `k`, and
/// return its claims: for the mock push service in tests.
#[doc(hidden)]
pub fn verify_vapid(header: &str) -> Result<serde_json::Value, String> {
    let rest = header
        .strip_prefix("vapid ")
        .ok_or_else(|| "not a vapid header".to_owned())?;
    let mut token = None;
    let mut key = None;
    for part in rest.split(',') {
        let part = part.trim();
        if let Some(t) = part.strip_prefix("t=") {
            token = Some(t);
        } else if let Some(k) = part.strip_prefix("k=") {
            key = Some(k);
        }
    }
    let (token, key) = token
        .zip(key)
        .ok_or_else(|| "t= or k= missing".to_owned())?;
    let (input, signature) = token
        .rsplit_once('.')
        .ok_or_else(|| "not a JWT".to_owned())?;
    let public = decode_b64(key)?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, &public)
        .verify(input.as_bytes(), &decode_b64(signature)?)
        .map_err(|_| "bad signature".to_owned())?;
    let (header, claims) = input
        .split_once('.')
        .ok_or_else(|| "not a JWT".to_owned())?;
    let header: serde_json::Value =
        serde_json::from_slice(&decode_b64(header)?).map_err(|e| e.to_string())?;
    if header["alg"] != "ES256" {
        return Err("not ES256".into());
    }
    serde_json::from_slice(&decode_b64(claims)?).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------
// Push services

/// Whether a subscription may name `endpoint`: `https://` to a configured
/// push service host (`host`, or `*.suffix`), or `http://` to a loopback
/// literal the configuration names (for a local test service). The list
/// keeps a token holder from making the server post to arbitrary hosts.
pub fn endpoint_allowed(endpoint: &str, services: &[String]) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(endpoint).map_err(|e| format!("endpoint: {e}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| "the endpoint has no host".to_owned())?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    let listed = services.iter().any(|service| {
        let service = service.to_ascii_lowercase();
        match service.strip_prefix("*.") {
            Some(suffix) => host.ends_with(&format!(".{suffix}")),
            None => host == service,
        }
    });
    if !listed {
        return Err(format!(
            "{host} is not a push service this server sends to (configure app.push_services)"
        ));
    }
    let loopback = matches!(host.as_str(), "127.0.0.1" | "::1" | "localhost");
    match url.scheme() {
        "https" => Ok(url),
        "http" if loopback => Ok(url),
        _ => Err("a push endpoint must be https://".into()),
    }
}

/// What one push attempt came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Sent {
    Delivered,
    /// The service says the subscription is gone (404, 410): drop it.
    Gone,
    Failed(String),
}

/// Send `notice` to `subscription`.
pub async fn send(
    client: &reqwest::Client,
    vapid: &Vapid,
    subscription: &Subscription,
    notice: &Notice,
) -> Sent {
    let url = match reqwest::Url::parse(&subscription.endpoint) {
        Ok(url) => url,
        Err(e) => return Sent::Failed(format!("endpoint: {e}")),
    };
    let payload = serde_json::to_vec(notice).unwrap_or_default();
    let body = match encrypt(&payload, &subscription.p256dh, &subscription.auth) {
        Ok(body) => body,
        Err(e) => return Sent::Failed(e),
    };
    let authorization = match vapid.authorization(&url, crate::ops::now_ms() / 1000) {
        Ok(a) => a,
        Err(e) => return Sent::Failed(e),
    };
    let urgency = match notice.kind {
        "permission" | "question" | "failed" => "high",
        _ => "normal",
    };
    let response = client
        .post(url)
        .header("Authorization", authorization)
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("TTL", TTL_SECONDS.to_string())
        .header("Urgency", urgency)
        .timeout(REQUEST_TIMEOUT)
        .body(body)
        .send()
        .await;
    match response {
        Ok(r) if r.status().is_success() => Sent::Delivered,
        Ok(r) if matches!(r.status().as_u16(), 404 | 410) => Sent::Gone,
        Ok(r) => Sent::Failed(format!("the push service answered {}", r.status())),
        Err(e) => Sent::Failed(format!("could not reach the push service: {e}")),
    }
}

/// The principal a subscription's credential verifies as now, if any.
pub(crate) async fn owner(app: &Arc<App>, token_sha256: &str) -> Option<Principal> {
    if let Some(principal) = app.credentials.principal_for_hash(token_sha256) {
        return Some(principal.clone());
    }
    let companion = app.companion.clone()?;
    let hash = token_sha256.to_owned();
    let token = tokio::task::spawn_blocking(move || companion.store.token(&hash))
        .await
        .ok()?
        .ok()??;
    token
        .usable_at(crate::ops::now_ms())
        .then_some(token.principal)
}

/// Send `notice` to every subscription whose owner may read `notice.repo`
/// and wants its kind; drop subscriptions that are gone.
pub(crate) async fn fan_out(
    app: &Arc<App>,
    notice: &Notice,
    only: Option<&[String]>,
) -> (usize, Vec<String>) {
    let Some(companion) = app.companion.clone() else {
        return (0, Vec::new());
    };
    let (Some(vapid), Some(client)) = (companion.vapid.as_ref(), companion.client.as_ref()) else {
        return (0, vec!["push is off on this server".into()]);
    };
    let store = companion.store.clone();
    let subscriptions = match tokio::task::spawn_blocking(move || store.subscriptions()).await {
        Ok(Ok(s)) => s,
        _ => return (0, vec!["could not read the subscriptions".into()]),
    };
    let mut delivered = 0;
    let mut failures = Vec::new();
    for subscription in subscriptions {
        if only.is_some_and(|o| !o.contains(&subscription.token_sha256)) {
            continue;
        }
        if !subscription.kinds.is_empty() && !subscription.kinds.iter().any(|k| k == notice.kind) {
            continue;
        }
        let Some(principal) = owner(app, &subscription.token_sha256).await else {
            drop_subscription(&companion, &subscription.endpoint).await;
            continue;
        };
        let policy = app.config.tenant_policy(&principal.tenant);
        if !principal.allows("read") || !principal.repo_allowed(&policy, &notice.repo) {
            continue;
        }
        if endpoint_allowed(&subscription.endpoint, &app.config.app.push_services).is_err() {
            drop_subscription(&companion, &subscription.endpoint).await;
            continue;
        }
        match send(client, vapid, &subscription, notice).await {
            Sent::Delivered => delivered += 1,
            Sent::Gone => {
                drop_subscription(&companion, &subscription.endpoint).await;
                failures.push("a subscription was gone; dropped it".into());
            }
            Sent::Failed(why) => {
                tracing::warn!(repo = %notice.repo, %why, "companion push not delivered");
                failures.push(why);
            }
        }
    }
    (delivered, failures)
}

async fn drop_subscription(companion: &super::Companion, endpoint: &str) {
    let (store, endpoint) = (companion.store.clone(), endpoint.to_owned());
    let _ = tokio::task::spawn_blocking(move || store.unsubscribe(&endpoint, None)).await;
}

/// One notifier per served repository, following its feed.
pub fn spawn(
    app: Arc<App>,
    store: Arc<dyn OperationStore>,
    shutdown: watch::Receiver<bool>,
) -> Vec<tokio::task::JoinHandle<()>> {
    app.repos
        .values()
        .map(|repo| {
            tokio::spawn(run(
                app.clone(),
                repo.clone(),
                store.clone(),
                shutdown.clone(),
            ))
        })
        .collect()
}

async fn run(
    app: Arc<App>,
    repo: RepoState,
    store: Arc<dyn OperationStore>,
    mut shutdown: watch::Receiver<bool>,
) {
    let cursor_id = format!("{}:companion-push", repo.name);
    let mut cursor = {
        let (store, id) = (store.clone(), cursor_id.clone());
        match tokio::task::spawn_blocking(move || store.load_webhook_cursor(&id)).await {
            Ok(Ok(Some(cursor))) if cursor <= repo.feed.head() => cursor,
            _ => repo.feed.head(),
        }
    };
    let mut tracker = Tracker::default();
    let mut head = repo.feed.subscribe();
    loop {
        loop {
            let (feed, from) = (repo.feed.clone(), cursor);
            let entries: Vec<FeedEntry> = match tokio::task::spawn_blocking(move || {
                feed.read_after(from, BATCH)
            })
            .await
            {
                Ok(Ok(entries)) => entries,
                Ok(Err(e)) => {
                    tracing::warn!(repo = %repo.name, error = %e, "companion push: reading the feed");
                    break;
                }
                Err(_) => return,
            };
            let Some(last) = entries.last().map(|e| e.seq) else {
                break;
            };
            let now = crate::ops::now_ms();
            for entry in &entries {
                if now.saturating_sub(entry.at_ms) > FRESH_MS {
                    continue;
                }
                if let Some(notice) = tracker.observe(&repo.name, &entry.branch, &entry.activity) {
                    fan_out(&app, &notice, None).await;
                }
            }
            cursor = last;
            let (store, id) = (store.clone(), cursor_id.clone());
            let _ = tokio::task::spawn_blocking(move || store.save_webhook_cursor(&id, last)).await;
        }
        if *shutdown.borrow() {
            return;
        }
        tokio::select! {
            _ = head.changed() => {}
            _ = tokio::time::sleep(POLL) => {}
            _ = shutdown.changed() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browser() -> (agreement::EphemeralPrivateKey, Vec<u8>, [u8; 16]) {
        let rng = SystemRandom::new();
        let private =
            agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let public = private.compute_public_key().unwrap().as_ref().to_vec();
        let mut auth = [0u8; 16];
        rng.fill(&mut auth).unwrap();
        (private, public, auth)
    }

    #[test]
    fn a_message_decrypts_as_a_browser_would() {
        let (private, public, auth) = browser();
        let payload = br#"{"title":"app: parser","body":"parser asks to use Bash"}"#;
        let body = encrypt(
            payload,
            &URL_SAFE_NO_PAD.encode(&public),
            &URL_SAFE_NO_PAD.encode(auth),
        )
        .unwrap();
        // salt, record size 4096, a 65-byte key id, then the record.
        assert_eq!(&body[16..20], &4096u32.to_be_bytes());
        assert_eq!(body[20], 65);
        assert_eq!(body.len(), 16 + 4 + 1 + 65 + payload.len() + 1 + 16);
        assert_eq!(
            decrypt_with(&body, private, &public, &auth).unwrap(),
            payload
        );
    }

    #[test]
    fn a_tampered_message_or_wrong_secret_does_not_decrypt() {
        let (private, public, auth) = browser();
        let mut body = encrypt(
            b"hello",
            &URL_SAFE_NO_PAD.encode(&public),
            &URL_SAFE_NO_PAD.encode(auth),
        )
        .unwrap();
        let last = body.len() - 1;
        body[last] ^= 1;
        assert!(decrypt_with(&body, private, &public, &auth).is_err());
        assert!(encrypt(b"x", "AAAA", &URL_SAFE_NO_PAD.encode(auth)).is_err());
        assert!(encrypt(b"x", &URL_SAFE_NO_PAD.encode(&public), "AAAA").is_err());
    }

    #[test]
    fn vapid_tokens_verify_against_their_key() {
        let vapid = Vapid::ephemeral("mailto:ops@example.com");
        let url = reqwest::Url::parse("https://fcm.googleapis.com/fcm/send/abc").unwrap();
        let header = vapid.authorization(&url, 1_000).unwrap();
        let claims = verify_vapid(&header).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["sub"], "mailto:ops@example.com");
        assert_eq!(claims["exp"], 1_000 + 43_200);
        assert!(header.ends_with(&format!("k={}", vapid.public_key())));
        let other = Vapid::ephemeral("mailto:x@example.com");
        let forged = header.replace(&vapid.public_key(), &other.public_key());
        assert!(verify_vapid(&forged).is_err());
        assert_eq!(decode_b64(&vapid.public_key()).unwrap().len(), 65);
    }

    #[test]
    fn endpoints_must_be_listed_push_services() {
        let services: Vec<String> = [
            "fcm.googleapis.com",
            "*.push.services.mozilla.com",
            "127.0.0.1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(endpoint_allowed("https://fcm.googleapis.com/fcm/send/x", &services).is_ok());
        assert!(endpoint_allowed(
            "https://updates.push.services.mozilla.com/wpush/v2/x",
            &services
        )
        .is_ok());
        assert!(endpoint_allowed("http://127.0.0.1:9/push/x", &services).is_ok());
        for bad in [
            "http://fcm.googleapis.com/x",
            "https://evil.example/x",
            "https://push.services.mozilla.com.evil.example/x",
            "https://169.254.169.254/latest",
            "http://localhost/x",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(endpoint_allowed(bad, &services).is_err(), "{bad}");
        }
    }

    #[test]
    fn notices_follow_by_watchs_rules() {
        let mut t = Tracker::default();
        let status = |s| Activity::Status(s);
        assert_eq!(t.observe("app", "a", &status(BranchStatus::Running)), None);
        let ready = t.observe("app", "a", &status(BranchStatus::Ready)).unwrap();
        assert_eq!(ready.kind, "finished");
        assert_eq!(ready.url, "#/b/app/a");
        assert_eq!(ready.title, "app: a");
        // The same status again (as recovery may record it) is not news.
        assert_eq!(t.observe("app", "a", &status(BranchStatus::Ready)), None);
        assert_eq!(t.observe("app", "a", &status(BranchStatus::Running)), None);
        let failed = t
            .observe(
                "app",
                "a",
                &status(BranchStatus::Failed {
                    reason: "exit 1".into(),
                }),
            )
            .unwrap();
        assert_eq!(
            (failed.kind, failed.body.as_str()),
            ("failed", "a failed: exit 1")
        );
        let stall = Activity::Stalled { since_ms: 5 };
        assert_eq!(t.observe("app", "a", &stall).unwrap().kind, "stalled");
        assert_eq!(t.observe("app", "a", &stall), None);
        let plan = t
            .observe("app", "p", &status(BranchStatus::AwaitingPlanApproval))
            .unwrap();
        assert_eq!(
            (plan.kind, plan.body.as_str()),
            ("question", "p has a plan waiting for your approval")
        );
        let message = Activity::Message(branchyard::Message {
            id: 9,
            from: "child".into(),
            to: "parent".into(),
            kind: MessageKind::Escalation,
            text: "need a decision".into(),
            in_reply_to: None,
            at_ms: 1,
            delivered: false,
        });
        let question = t.observe("app", "child", &message).unwrap();
        assert_eq!(question.kind, "question");
        assert_eq!(question.branch, "parent");
        assert_eq!(question.url, "#/inbox");
        assert_eq!(question.body, "child escalates to parent: need a decision");
        // Recorded on both branches' logs: one notice.
        assert_eq!(t.observe("app", "parent", &message), None);
        assert_eq!(url_part("a b/ü"), "a%20b%2F%C3%BC");
        for kind in KINDS {
            check_kind(kind).unwrap();
        }
        assert!(check_kind("everything").is_err());
    }
}
