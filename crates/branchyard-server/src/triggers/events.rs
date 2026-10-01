//! Webhook deliveries from GitHub, Slack, Linear and generic senders:
//! verified with the trigger's secret, then read into one
//! [`TriggerEvent`] shape that conditions and templates use.
//!
//! | Source | Signature | Replay window | Event ID |
//! |---|---|---|---|
//! | GitHub | `X-Hub-Signature-256: sha256=HMAC(secret, body)` | none (GitHub signs no timestamp) | a hash of the body |
//! | Slack | `X-Slack-Signature: v0=HMAC(secret, "v0:" ts ":" body)` | `X-Slack-Request-Timestamp` | `event_id` |
//! | Linear | `Linear-Signature: HMAC(secret, body)` | `webhookTimestamp` in the body | a hash of the body |
//! | Generic | `X-Branchyard-Signature: sha256=HMAC(secret, body)` | none | the body's `id`, else a hash of the body |
//!
//! Every HMAC is SHA-256, hex-encoded, compared in constant time. An
//! event's ID comes only from the signed bytes: a delivery ID header is not
//! signed, so a captured delivery replayed under a new one would otherwise
//! be a new event. It is kept as `delivery`, for looking a delivery up at
//! its sender. A sender's own redelivery sends the same body, and is the
//! same event.

use branchyard_client::triggers::{EventSource, TriggerEvent};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// What a verified delivery asks for.
#[derive(Clone, Debug, PartialEq)]
pub enum Delivery {
    /// Slack's URL verification: answer with this challenge.
    Challenge(String),
    /// Nothing to fire on (a ping, an event kind no adapter reads).
    Ignored(String),
    Event(Box<TriggerEvent>),
}

/// Why a delivery was refused: `401` with this code and message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub code: &'static str,
    pub message: String,
}

fn refused(code: &'static str, message: impl Into<String>) -> Refused {
    Refused {
        code,
        message: message.into(),
    }
}

/// HMAC-SHA256 of `parts`, concatenated, with `secret`, as lowercase hex.
pub fn sign(secret: &str, parts: &[&[u8]]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("any key length");
    for part in parts {
        mac.update(part);
    }
    hex::encode(mac.finalize().into_bytes())
}

/// Whether `hex_signature` is the HMAC of `parts` under `secret`,
/// compared in constant time.
fn verify(secret: &str, parts: &[&[u8]], hex_signature: &str) -> bool {
    let Ok(given) = hex::decode(hex_signature.trim()) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("any key length");
    for part in parts {
        mac.update(part);
    }
    mac.verify_slice(&given).is_ok()
}

/// `application/x-www-form-urlencoded` value bytes, decoded.
fn form_decode(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len());
    let mut i = 0;
    while i < value.len() {
        match value[i] {
            b'&' => break,
            b'+' => out.push(b' '),
            b'%' => {
                let hex = value
                    .get(i + 1..i + 3)
                    .and_then(|h| std::str::from_utf8(h).ok());
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    out
}

fn body_hash(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))[..32].to_owned()
}

/// Verify a delivery of `source` and read it. `header` looks a request
/// header up by its lowercase name; `now_ms` and `window_seconds` bound
/// the age of a Slack or Linear delivery's own timestamp.
pub fn receive(
    source: EventSource,
    header: &dyn Fn(&str) -> Option<String>,
    body: &[u8],
    secret: &str,
    now_ms: u64,
    window_seconds: u64,
) -> Result<Delivery, Refused> {
    let bad_signature = || {
        refused(
            "invalid_signature",
            "the delivery's signature does not match this trigger's secret",
        )
    };
    let fresh = |at_ms: u64| {
        let age = now_ms.abs_diff(at_ms);
        match age <= window_seconds.saturating_mul(1000) {
            true => Ok(()),
            false => Err(refused(
                "stale_delivery",
                format!(
                    "the delivery's timestamp is {}s from this server's clock, outside the \
                     {window_seconds}s replay window",
                    age / 1000
                ),
            )),
        }
    };
    match source {
        EventSource::Github => {
            let signature = header("x-hub-signature-256").ok_or_else(bad_signature)?;
            let hex = signature
                .strip_prefix("sha256=")
                .ok_or_else(bad_signature)?;
            if !verify(secret, &[body], hex) {
                return Err(bad_signature());
            }
        }
        EventSource::Generic => {
            let signature = header("x-branchyard-signature").ok_or_else(bad_signature)?;
            let hex = signature
                .strip_prefix("sha256=")
                .ok_or_else(bad_signature)?;
            if !verify(secret, &[body], hex) {
                return Err(bad_signature());
            }
        }
        EventSource::Slack => {
            let signature = header("x-slack-signature").ok_or_else(bad_signature)?;
            let ts = header("x-slack-request-timestamp").ok_or_else(bad_signature)?;
            let hex = signature.strip_prefix("v0=").ok_or_else(bad_signature)?;
            if !verify(secret, &[b"v0:", ts.as_bytes(), b":", body], hex) {
                return Err(bad_signature());
            }
            let seconds: u64 = ts.trim().parse().map_err(|_| bad_signature())?;
            fresh(seconds.saturating_mul(1000))?;
        }
        EventSource::Linear => {
            let signature = header("linear-signature").ok_or_else(bad_signature)?;
            if !verify(secret, &[body], &signature) {
                return Err(bad_signature());
            }
        }
    }
    let payload: Value = match serde_json::from_slice(body) {
        Ok(payload) => payload,
        // GitHub's other content type: `payload=<URL-encoded JSON>`.
        Err(_) if source == EventSource::Github && body.starts_with(b"payload=") => {
            serde_json::from_slice(&form_decode(&body[b"payload=".len()..]))
                .map_err(|e| refused("invalid_request", format!("the payload is not JSON: {e}")))?
        }
        Err(e) => {
            return Err(refused(
                "invalid_request",
                format!("the body is not JSON: {e}"),
            ))
        }
    };
    if source == EventSource::Linear {
        let at = payload
            .get("webhookTimestamp")
            .and_then(Value::as_u64)
            .ok_or_else(|| refused("stale_delivery", "the delivery has no webhookTimestamp"))?;
        fresh(at)?;
    }
    let delivery = match source {
        EventSource::Github => header("x-github-delivery"),
        EventSource::Linear => header("linear-delivery"),
        EventSource::Generic => header("x-branchyard-event-id"),
        EventSource::Slack => None,
    };
    let kind = match source {
        EventSource::Github => header("x-github-event"),
        _ => None,
    };
    read(source, kind.as_deref(), delivery.as_deref(), &payload, body)
        .map_err(|e| refused("invalid_request", e))
}

/// Read a delivery body of `source` without verifying it: for a trigger
/// test, and after [`receive`] verified it. `kind` is GitHub's
/// `X-GitHub-Event` (inferred from the body's shape when `None`);
/// `delivery` the delivery's ID header, if any.
pub fn read(
    source: EventSource,
    kind: Option<&str>,
    delivery: Option<&str>,
    payload: &Value,
    body: &[u8],
) -> Result<Delivery, String> {
    if !payload.is_object() {
        return Err("the body is not a JSON object".into());
    }
    let id = body_hash(body);
    let read = match source {
        EventSource::Github => github(kind, id, payload),
        EventSource::Slack => slack(payload, id),
        EventSource::Linear => linear(payload, id),
        EventSource::Generic => Ok(generic(payload, id)),
    };
    read.map(|delivered| match delivered {
        Delivery::Event(mut e) => {
            e.delivery = delivery.filter(|d| !d.trim().is_empty()).map(str::to_owned);
            Delivery::Event(e)
        }
        other => other,
    })
}

fn text(value: &Value, path: &[&str]) -> Option<String> {
    let mut v = value;
    for key in path {
        v = v.get(key)?;
    }
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn label_names(value: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(labels)) = value else {
        return Vec::new();
    };
    labels
        .iter()
        .filter_map(|l| match l {
            Value::String(s) => Some(s.clone()),
            other => text(other, &["name"]),
        })
        .collect()
}

fn event(source: EventSource, kind: String, id: String, payload: &Value) -> TriggerEvent {
    TriggerEvent {
        source: source.as_str().into(),
        kind,
        id,
        payload: payload.clone(),
        ..TriggerEvent::default()
    }
}

/// GitHub's event type from a body's shape, for a test without one.
fn infer_github(payload: &Value) -> &'static str {
    let has = |key: &str| payload.get(key).is_some();
    if has("zen") {
        "ping"
    } else if has("comment") && has("issue") {
        "issue_comment"
    } else if has("pull_request") {
        "pull_request"
    } else if has("check_suite") {
        "check_suite"
    } else if has("issue") {
        "issues"
    } else {
        "unknown"
    }
}

fn github(kind: Option<&str>, id: String, p: &Value) -> Result<Delivery, String> {
    let kind = kind.unwrap_or_else(|| infer_github(p));
    let action = text(p, &["action"]).unwrap_or_default();
    let mut e = event(EventSource::Github, String::new(), id, p);
    e.repo = text(p, &["repository", "full_name"]);
    e.author = text(p, &["sender", "login"]);
    match kind {
        "ping" => return Ok(Delivery::Ignored("a GitHub ping".into())),
        "issues" => {
            e.kind = format!("issues.{action}");
            e.title = text(p, &["issue", "title"]);
            e.text = text(p, &["issue", "body"]);
            e.url = text(p, &["issue", "html_url"]);
            e.number = text(p, &["issue", "number"]);
            e.labels = label_names(p.pointer("/issue/labels"));
            if let Some(added) = text(p, &["label", "name"]) {
                if !e.labels.contains(&added) {
                    e.labels.push(added);
                }
            }
        }
        "issue_comment" => {
            e.kind = format!("issue_comment.{action}");
            e.title = text(p, &["issue", "title"]);
            e.text = text(p, &["comment", "body"]);
            e.url = text(p, &["comment", "html_url"]);
            e.number = text(p, &["issue", "number"]);
            e.labels = label_names(p.pointer("/issue/labels"));
            e.author = text(p, &["comment", "user", "login"]).or(e.author);
        }
        "pull_request" => {
            e.kind = format!("pull_request.{action}");
            e.title = text(p, &["pull_request", "title"]);
            e.text = text(p, &["pull_request", "body"]);
            e.url = text(p, &["pull_request", "html_url"]);
            e.number = text(p, &["pull_request", "number"]);
            e.branch = text(p, &["pull_request", "head", "ref"]);
            e.labels = label_names(p.pointer("/pull_request/labels"));
        }
        "check_suite" => {
            let conclusion = text(p, &["check_suite", "conclusion"]);
            e.kind = match (action.as_str(), &conclusion) {
                ("completed", Some(c)) => format!("check_suite.{c}"),
                _ => format!("check_suite.{action}"),
            };
            e.branch = text(p, &["check_suite", "head_branch"]);
            e.text = text(p, &["check_suite", "head_commit", "message"]);
            e.title = Some(format!(
                "check suite {} on {}",
                conclusion.as_deref().unwrap_or(&action),
                e.branch.as_deref().unwrap_or("a commit")
            ));
            e.number = p
                .pointer("/check_suite/pull_requests/0/number")
                .map(|n| n.to_string());
            if let (Some(repo), Some(sha)) = (
                text(p, &["repository", "html_url"]),
                text(p, &["check_suite", "head_sha"]),
            ) {
                e.url = Some(format!("{repo}/commit/{sha}/checks"));
            }
        }
        other => {
            return Ok(Delivery::Ignored(format!(
                "GitHub's {other} event is not one triggers read (issues, issue_comment, \
                 pull_request, check_suite)"
            )))
        }
    }
    Ok(Delivery::Event(Box::new(e)))
}

fn slack(p: &Value, id: String) -> Result<Delivery, String> {
    match text(p, &["type"]).as_deref() {
        Some("url_verification") => {
            let challenge =
                text(p, &["challenge"]).ok_or("url_verification without a challenge")?;
            Ok(Delivery::Challenge(challenge))
        }
        Some("event_callback") => {
            let kind = text(p, &["event", "type"]).unwrap_or_default();
            if kind != "app_mention" {
                return Ok(Delivery::Ignored(format!(
                    "Slack's {kind} event is not one triggers read (app_mention)"
                )));
            }
            let id = text(p, &["event_id"]).unwrap_or(id);
            let mut e = event(EventSource::Slack, kind, id, p);
            e.author = text(p, &["event", "user"]);
            e.text = text(p, &["event", "text"]);
            e.channel = text(p, &["event", "channel"]);
            e.repo = text(p, &["team_id"]);
            Ok(Delivery::Event(Box::new(e)))
        }
        other => Ok(Delivery::Ignored(format!(
            "a Slack request of type {} is not one triggers read",
            other.unwrap_or("(none)")
        ))),
    }
}

fn linear(p: &Value, id: String) -> Result<Delivery, String> {
    let kind = text(p, &["type"]).unwrap_or_default();
    if kind != "Issue" {
        return Ok(Delivery::Ignored(format!(
            "Linear's {kind} webhook is not one triggers read (Issue)"
        )));
    }
    let action = text(p, &["action"]).unwrap_or_default();
    let mut e = event(EventSource::Linear, format!("issue.{action}"), id, p);
    e.title = text(p, &["data", "title"]);
    e.text = text(p, &["data", "description"]);
    e.url = text(p, &["data", "url"]).or_else(|| text(p, &["url"]));
    e.number = text(p, &["data", "identifier"]);
    e.repo = text(p, &["data", "team", "key"]);
    e.author = text(p, &["actor", "name"]).or_else(|| text(p, &["data", "creator", "name"]));
    e.labels = label_names(p.pointer("/data/labels"));
    Ok(Delivery::Event(Box::new(e)))
}

fn generic(p: &Value, id: String) -> Delivery {
    let id = text(p, &["id"]).unwrap_or(id);
    let mut e = event(
        EventSource::Generic,
        text(p, &["kind"]).unwrap_or_else(|| "event".into()),
        id,
        p,
    );
    e.repo = text(p, &["repo"]);
    e.author = text(p, &["author"]);
    e.title = text(p, &["title"]);
    e.text = text(p, &["text"]);
    e.url = text(p, &["url"]);
    e.number = text(p, &["number"]);
    e.branch = text(p, &["branch"]);
    e.channel = text(p, &["channel"]);
    e.labels = label_names(p.get("labels"));
    Delivery::Event(Box::new(e))
}

/// Event kinds a condition may name, per source: a prefix with `*` is
/// also accepted.
pub fn known_kind(source: EventSource, kind: &str) -> bool {
    let prefix = kind.strip_suffix('*').unwrap_or(kind);
    let families: &[&str] = match source {
        EventSource::Github => &["issues.", "issue_comment.", "pull_request.", "check_suite."],
        EventSource::Slack => &["app_mention"],
        EventSource::Linear => &["issue."],
        EventSource::Generic => return !kind.is_empty(),
    };
    families
        .iter()
        .any(|f| prefix.starts_with(f) || f.starts_with(prefix) && !prefix.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn headers(pairs: &[(&str, String)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    const NOW: u64 = 1_790_000_000_000;

    fn issue_labeled() -> Value {
        serde_json::json!({
            "action": "labeled",
            "label": {"name": "agent"},
            "issue": {"number": 42, "title": "Parser crash", "body": "It crashes on empty input",
                      "html_url": "https://github.com/acme/app/issues/42",
                      "labels": [{"name": "bug"}, {"name": "agent"}]},
            "repository": {"full_name": "acme/app", "html_url": "https://github.com/acme/app"},
            "sender": {"login": "alice"}
        })
    }

    #[test]
    fn github_signatures_are_checked_and_issues_read() {
        let body = serde_json::to_vec(&issue_labeled()).unwrap();
        let good = format!("sha256={}", sign("s3cret", &[&body]));
        let h = headers(&[
            ("x-hub-signature-256", good),
            ("x-github-event", "issues".into()),
            ("x-github-delivery", "d-1".into()),
        ]);
        let Delivery::Event(e) =
            receive(EventSource::Github, &h, &body, "s3cret", NOW, 300).unwrap()
        else {
            panic!("an event")
        };
        assert_eq!(e.kind, "issues.labeled");
        assert_eq!(e.delivery.as_deref(), Some("d-1"));
        // The ID is the signed body's, not the unsigned header's: the same
        // body under another delivery ID is the same event.
        assert_eq!(e.id.len(), 32);
        let other = headers(&[
            (
                "x-hub-signature-256",
                format!("sha256={}", sign("s3cret", &[&body])),
            ),
            ("x-github-event", "issues".into()),
            ("x-github-delivery", "d-replayed".into()),
        ]);
        let Delivery::Event(again) =
            receive(EventSource::Github, &other, &body, "s3cret", NOW, 300).unwrap()
        else {
            panic!("an event")
        };
        assert_eq!(again.id, e.id);
        assert_eq!(e.repo.as_deref(), Some("acme/app"));
        assert_eq!(e.number.as_deref(), Some("42"));
        assert_eq!(e.author.as_deref(), Some("alice"));
        assert_eq!(e.labels, ["bug", "agent"]);

        let wrong = format!("sha256={}", sign("other", &[&body]));
        let h = headers(&[
            ("x-hub-signature-256", wrong),
            ("x-github-event", "issues".into()),
        ]);
        let err = receive(EventSource::Github, &h, &body, "s3cret", NOW, 300).unwrap_err();
        assert_eq!(err.code, "invalid_signature");
        let missing = headers(&[("x-github-event", "issues".into())]);
        assert!(receive(EventSource::Github, &missing, &body, "s3cret", NOW, 300).is_err());
        // A tampered body fails with the original's signature.
        let mut tampered = body.clone();
        tampered[10] ^= 1;
        let h = headers(&[(
            "x-hub-signature-256",
            format!("sha256={}", sign("s3cret", &[&body])),
        )]);
        assert!(receive(EventSource::Github, &h, &tampered, "s3cret", NOW, 300).is_err());
    }

    #[test]
    fn github_form_encoded_payloads_are_read() {
        let json = serde_json::to_string(&issue_labeled()).unwrap();
        let mut body = b"payload=".to_vec();
        for byte in json.bytes() {
            match byte {
                b' ' => body.push(b'+'),
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' => body.push(byte),
                other => body.extend(format!("%{other:02X}").bytes()),
            }
        }
        let h = headers(&[
            (
                "x-hub-signature-256",
                format!("sha256={}", sign("s", &[&body])),
            ),
            ("x-github-event", "issues".into()),
        ]);
        let Delivery::Event(e) = receive(EventSource::Github, &h, &body, "s", NOW, 300).unwrap()
        else {
            panic!("an event")
        };
        assert_eq!(e.title.as_deref(), Some("Parser crash"));
    }

    #[test]
    fn github_comments_pull_requests_check_suites_and_pings() {
        let comment = serde_json::json!({
            "action": "created",
            "issue": {"number": 7, "title": "Flaky test", "labels": []},
            "comment": {"body": "@branchyard please fix", "user": {"login": "bob"},
                        "html_url": "https://github.com/acme/app/issues/7#c1"},
            "repository": {"full_name": "acme/app"}, "sender": {"login": "bob"}
        });
        let Delivery::Event(e) = read(EventSource::Github, None, None, &comment, b"x").unwrap()
        else {
            panic!()
        };
        assert_eq!(e.kind, "issue_comment.created");
        assert_eq!(e.text.as_deref(), Some("@branchyard please fix"));
        assert_eq!(e.author.as_deref(), Some("bob"));

        let pr = serde_json::json!({
            "action": "opened",
            "pull_request": {"number": 9, "title": "Add X", "body": "", "head": {"ref": "feature/x"},
                             "labels": [{"name": "agent"}]},
            "repository": {"full_name": "acme/app"}, "sender": {"login": "carol"}
        });
        let Delivery::Event(e) = read(EventSource::Github, None, None, &pr, b"y").unwrap() else {
            panic!()
        };
        assert_eq!(e.kind, "pull_request.opened");
        assert_eq!(e.branch.as_deref(), Some("feature/x"));

        let suite = serde_json::json!({
            "action": "completed",
            "check_suite": {"conclusion": "failure", "head_branch": "main", "head_sha": "abc",
                            "pull_requests": [{"number": 3}], "head_commit": {"message": "wip"}},
            "repository": {"full_name": "acme/app", "html_url": "https://github.com/acme/app"},
            "sender": {"login": "ci"}
        });
        let Delivery::Event(e) = read(EventSource::Github, None, None, &suite, b"z").unwrap()
        else {
            panic!()
        };
        assert_eq!(e.kind, "check_suite.failure");
        assert_eq!(e.branch.as_deref(), Some("main"));
        assert_eq!(e.number.as_deref(), Some("3"));
        assert_eq!(
            e.url.as_deref(),
            Some("https://github.com/acme/app/commit/abc/checks")
        );

        let ping = serde_json::json!({"zen": "Keep it logically awesome.", "hook_id": 1});
        assert!(matches!(
            read(EventSource::Github, None, None, &ping, b"p").unwrap(),
            Delivery::Ignored(_)
        ));
        let push = serde_json::json!({"ref": "refs/heads/main"});
        assert!(matches!(
            read(EventSource::Github, Some("push"), None, &push, b"q").unwrap(),
            Delivery::Ignored(_)
        ));
    }

    #[test]
    fn slack_verification_mentions_and_replay_window() {
        let ts = (NOW / 1000).to_string();
        let sign_slack = |body: &[u8], ts: &str| {
            format!(
                "v0={}",
                sign("slack-secret", &[b"v0:", ts.as_bytes(), b":", body])
            )
        };
        let challenge = br#"{"type":"url_verification","challenge":"abc123","token":"x"}"#;
        let h = headers(&[
            ("x-slack-signature", sign_slack(challenge, &ts)),
            ("x-slack-request-timestamp", ts.clone()),
        ]);
        assert_eq!(
            receive(EventSource::Slack, &h, challenge, "slack-secret", NOW, 300).unwrap(),
            Delivery::Challenge("abc123".into())
        );

        let mention = br#"{"type":"event_callback","team_id":"T1","event_id":"Ev1",
            "event":{"type":"app_mention","user":"U1","text":"<@B1> fix the build","channel":"C1"}}"#;
        let h = headers(&[
            ("x-slack-signature", sign_slack(mention, &ts)),
            ("x-slack-request-timestamp", ts.clone()),
        ]);
        let Delivery::Event(e) =
            receive(EventSource::Slack, &h, mention, "slack-secret", NOW, 300).unwrap()
        else {
            panic!()
        };
        assert_eq!((e.kind.as_str(), e.id.as_str()), ("app_mention", "Ev1"));
        assert_eq!(e.channel.as_deref(), Some("C1"));

        // Ten minutes old: refused even though signed.
        let old = (NOW / 1000 - 600).to_string();
        let h = headers(&[
            ("x-slack-signature", sign_slack(mention, &old)),
            ("x-slack-request-timestamp", old),
        ]);
        let err = receive(EventSource::Slack, &h, mention, "slack-secret", NOW, 300).unwrap_err();
        assert_eq!(err.code, "stale_delivery");
        // The timestamp is signed: changing it breaks the signature.
        let h = headers(&[
            ("x-slack-signature", sign_slack(mention, &ts)),
            ("x-slack-request-timestamp", (NOW / 1000 + 1).to_string()),
        ]);
        let err = receive(EventSource::Slack, &h, mention, "slack-secret", NOW, 300).unwrap_err();
        assert_eq!(err.code, "invalid_signature");
    }

    #[test]
    fn linear_issues_signatures_and_timestamps() {
        let body = serde_json::to_vec(&serde_json::json!({
            "type": "Issue", "action": "create", "webhookTimestamp": NOW - 1000,
            "actor": {"name": "Dana"},
            "data": {"identifier": "ENG-12", "title": "Slow query", "description": "p95 is 4s",
                     "url": "https://linear.app/acme/issue/ENG-12", "team": {"key": "ENG"},
                     "labels": [{"name": "agent"}]}
        }))
        .unwrap();
        let h = headers(&[
            ("linear-signature", sign("lin", &[&body])),
            ("linear-delivery", "ld-1".into()),
        ]);
        let Delivery::Event(e) = receive(EventSource::Linear, &h, &body, "lin", NOW, 60).unwrap()
        else {
            panic!()
        };
        assert_eq!(e.kind, "issue.create");
        assert_eq!(e.delivery.as_deref(), Some("ld-1"));
        assert_eq!(e.number.as_deref(), Some("ENG-12"));
        assert_eq!(e.repo.as_deref(), Some("ENG"));
        assert_eq!(e.labels, ["agent"]);
        let err = receive(EventSource::Linear, &h, &body, "lin", NOW + 120_000, 60).unwrap_err();
        assert_eq!(err.code, "stale_delivery");
        let h = headers(&[("linear-signature", sign("nope", &[&body]))]);
        assert!(receive(EventSource::Linear, &h, &body, "lin", NOW, 60).is_err());
    }

    #[test]
    fn generic_events_take_their_fields_and_ids() {
        let body = br#"{"id":"evt-9","kind":"deploy.failed","repo":"acme/app","labels":["prod"],"text":"boom"}"#;
        let h = headers(&[(
            "x-branchyard-signature",
            format!("sha256={}", sign("g", &[body])),
        )]);
        let Delivery::Event(e) = receive(EventSource::Generic, &h, body, "g", NOW, 300).unwrap()
        else {
            panic!()
        };
        assert_eq!((e.id.as_str(), e.kind.as_str()), ("evt-9", "deploy.failed"));
        assert_eq!(e.labels, ["prod"]);
        // Without an ID, the body's hash: the same body is the same event.
        let a = read(
            EventSource::Generic,
            None,
            None,
            &serde_json::json!({"x": 1}),
            b"{\"x\":1}",
        );
        let b = read(
            EventSource::Generic,
            None,
            None,
            &serde_json::json!({"x": 1}),
            b"{\"x\":1}",
        );
        assert_eq!(a, b);
    }

    #[test]
    fn known_kinds_per_source() {
        assert!(known_kind(EventSource::Github, "issues.opened"));
        assert!(known_kind(EventSource::Github, "pull_request.*"));
        assert!(known_kind(EventSource::Github, "check_suite.failure"));
        assert!(!known_kind(EventSource::Github, "push"));
        assert!(known_kind(EventSource::Slack, "app_mention"));
        assert!(!known_kind(EventSource::Slack, "message"));
        assert!(known_kind(EventSource::Linear, "issue.update"));
        assert!(known_kind(EventSource::Generic, "anything"));
    }
}
