//! Webhook notifications: an operator-configured target receives a signed
//! JSON envelope for each matching entry of a served repository's activity
//! feed — branch status changes (further tagged `merge` or `failure`),
//! stalls, and permission requests (`permission_wait`).
//!
//! Delivery is by cursor from the durable operation store
//! ([`OperationStore::load_webhook_cursor`],
//! [`OperationStore::save_webhook_cursor`]), keyed by `<repo>:<webhook id>`,
//! so a restart resumes after the last delivered position rather than
//! replaying the feed or silently skipping ahead: at-least-once. Delivery
//! is retried with backoff; a target that keeps refusing gets a
//! dead-letter note logged to stderr, and its cursor still advances past
//! that entry, so one broken target never blocks the others or the feed
//! itself. `X-Branchyard-Delivery` carries the feed position as the
//! receiver's dedupe key.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use backon::{ExponentialBuilder, Retryable};
use branchyard::{Activity, BranchStatus};
use branchyard_client::api::FeedEntry;
use hmac::{Hmac, KeyInit, Mac};
use serde::Serialize;
use sha2::Sha256;
use tokio::sync::watch;

use crate::api::RepoState;
use crate::config::WebhookConfig;
use crate::observe::Observability;
use crate::store::OperationStore;

/// Feed entries read per delivery batch.
const BATCH: usize = 100;
/// How long a quiet task sleeps between polls, beyond being woken by new
/// activity.
const POLL: Duration = Duration::from_secs(5);
/// Delivery attempts before a dead-letter note and moving on. Overridable
/// with `BY_TEST_WEBHOOK_MAX_ATTEMPTS` so a hermetic test can see
/// exhaustion without waiting through the production backoff.
const MAX_ATTEMPTS: u32 = 6;
const RETRY_BASE: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

fn max_attempts() -> u32 {
    static V: OnceLock<u32> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("BY_TEST_WEBHOOK_MAX_ATTEMPTS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(MAX_ATTEMPTS)
    })
}

/// Overridable with `BY_TEST_WEBHOOK_RETRY_MS`; see [`max_attempts`].
fn retry_base() -> Duration {
    static V: OnceLock<Duration> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("BY_TEST_WEBHOOK_RETRY_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Duration::from_millis)
            .unwrap_or(RETRY_BASE)
    })
}

/// A signed delivery: what a webhook receiver gets as its JSON body.
#[derive(Serialize)]
struct Envelope<'a> {
    repo: &'a str,
    seq: u64,
    branch: &'a str,
    at_ms: u64,
    kinds: &'a [&'static str],
    activity: &'a Activity,
}

/// The recognized kinds `activity` matches; see
/// [`crate::config::WEBHOOK_EVENT_KINDS`]. A status change is also `merge`
/// or `failure` when it settles there.
fn kinds_of(activity: &Activity) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    match activity {
        Activity::Status(status) => {
            kinds.push("status");
            match status {
                BranchStatus::Merged { .. } => kinds.push("merge"),
                BranchStatus::Failed { .. } => kinds.push("failure"),
                _ => {}
            }
        }
        Activity::Stalled { .. } => kinds.push("stall"),
        Activity::Harness(branchyard::Event::PermissionRequested { .. }) => {
            kinds.push("permission_wait")
        }
        _ => {}
    }
    kinds
}

/// Whether `webhook` wants an entry with these kinds; an empty filter wants
/// everything.
fn wants(webhook: &WebhookConfig, kinds: &[&str]) -> bool {
    webhook.events.is_empty() || kinds.iter().any(|k| webhook.events.contains(*k))
}

type HmacSha256 = Hmac<Sha256>;

/// Hex-encoded HMAC-SHA256 of `body` with `secret`.
fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts a key of any length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Start delivering `repo`'s feed to `webhook`, from its durably stored
/// cursor (the feed's current head if none is stored, so a webhook added
/// to a long-lived repository does not replay its whole history).
pub fn spawn(
    repo: RepoState,
    webhook: WebhookConfig,
    store: Arc<dyn OperationStore>,
    client: reqwest::Client,
    shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    spawn_observed(
        repo,
        webhook,
        store,
        client,
        Observability::default(),
        shutdown,
    )
}

/// [`spawn`], counting deliveries in `observability`'s metrics and sending
/// each with the `traceparent` of the operation that last worked on its
/// branch here, when there is one.
pub fn spawn_observed(
    repo: RepoState,
    webhook: WebhookConfig,
    store: Arc<dyn OperationStore>,
    client: reqwest::Client,
    observability: Observability,
    shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run(repo, webhook, store, client, observability, shutdown))
}

async fn run(
    repo: RepoState,
    webhook: WebhookConfig,
    store: Arc<dyn OperationStore>,
    client: reqwest::Client,
    observability: Observability,
    mut shutdown: watch::Receiver<bool>,
) {
    let cursor_id = format!("{}:{}", repo.name, webhook.id);
    let mut cursor = {
        let store = store.clone();
        let id = cursor_id.clone();
        let loaded = tokio::task::spawn_blocking(move || store.load_webhook_cursor(&id)).await;
        match loaded {
            Ok(Ok(Some(cursor))) => cursor,
            Ok(Ok(None)) => repo.feed.head(),
            Ok(Err(e)) => {
                tracing::error!(
                    webhook = %webhook.url,
                    repo = %repo.name,
                    error = %e,
                    "webhook: reading its cursor; starting from the current head"
                );
                repo.feed.head()
            }
            Err(_) => repo.feed.head(),
        }
    };
    loop {
        loop {
            let (feed, from) = (repo.feed.clone(), cursor);
            let entries = tokio::task::spawn_blocking(move || feed.read_after(from, BATCH)).await;
            let entries: Vec<FeedEntry> = match entries {
                Ok(Ok(entries)) => entries,
                Ok(Err(e)) => {
                    tracing::error!(
                        webhook = %webhook.url,
                        repo = %repo.name,
                        error = %e,
                        "webhook: reading the repo's feed"
                    );
                    break;
                }
                Err(_) => return,
            };
            if entries.is_empty() {
                break;
            }
            for entry in &entries {
                let kinds = kinds_of(&entry.activity);
                if wants(&webhook, &kinds) {
                    deliver(&client, &webhook, &repo.name, entry, &kinds, &observability).await;
                }
                cursor = entry.seq;
                let (store, id, cursor) = (store.clone(), cursor_id.clone(), cursor);
                let saved =
                    tokio::task::spawn_blocking(move || store.save_webhook_cursor(&id, cursor))
                        .await;
                if let Ok(Err(e)) = saved {
                    tracing::error!(
                        webhook = %webhook.url,
                        error = %e,
                        "webhook: saving its cursor"
                    );
                }
            }
            if *shutdown.borrow() {
                return;
            }
        }
        if *shutdown.borrow() {
            return;
        }
        tokio::select! {
            _ = repo.wake.notified() => {}
            _ = tokio::time::sleep(POLL) => {}
            _ = shutdown.changed() => {}
        }
    }
}

/// Deliver one entry, retrying with backoff ([`backoff`], through
/// `backon`); logs a warning for each failed attempt that will be retried,
/// then a dead-letter note with the last error after [`MAX_ATTEMPTS`].
async fn deliver(
    client: &reqwest::Client,
    webhook: &WebhookConfig,
    repo: &str,
    entry: &FeedEntry,
    kinds: &[&'static str],
    observability: &Observability,
) {
    let metrics = &observability.metrics;
    let traceparent = observability.tracer.branch_trace(repo, &entry.branch);
    let envelope = Envelope {
        repo,
        seq: entry.seq,
        branch: &entry.branch,
        at_ms: entry.at_ms,
        kinds,
        activity: &entry.activity,
    };
    let body = match serde_json::to_vec(&envelope) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!(
                webhook = %webhook.url,
                seq = entry.seq,
                error = %e,
                "webhook: could not encode a delivery"
            );
            return;
        }
    };
    let signature = sign(&webhook.secret, &body);
    let max_attempts = max_attempts();
    let attempt = || async {
        let mut request = client
            .post(&webhook.url)
            .header("content-type", "application/json")
            .header("x-branchyard-signature", format!("sha256={signature}"))
            .header("x-branchyard-delivery", entry.seq.to_string());
        if let Some(traceparent) = &traceparent {
            request = request.header("traceparent", traceparent.as_str());
        }
        let response = request
            .body(body.clone())
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(Refused::Transport)?;
        match response.status().is_success() {
            true => Ok(()),
            false => Err(Refused::Status(response.status())),
        }
    };
    let delivered = attempt
        .retry(backoff(max_attempts))
        .sleep(tokio::time::sleep)
        .notify(|error, wait| {
            metrics.inc(crate::metrics::WEBHOOKS, &[("result", "retried")]);
            tracing::warn!(
                webhook = %webhook.url,
                seq = entry.seq,
                error = %error,
                retry_in_ms = wait.as_millis() as u64,
                "webhook: delivery attempt failed"
            )
        })
        .await;
    let result = match delivered {
        Ok(()) => "delivered",
        Err(_) => "dead_lettered",
    };
    metrics.inc(crate::metrics::WEBHOOKS, &[("result", result)]);
    if let Err(error) = delivered {
        tracing::error!(
            webhook = %webhook.url,
            seq = entry.seq,
            attempts = max_attempts.max(1),
            error = %error,
            "webhook: dead-lettered a delivery; its cursor still advances past it"
        );
    }
}

/// Why one delivery attempt did not land.
#[derive(Debug)]
enum Refused {
    Transport(reqwest::Error),
    Status(reqwest::StatusCode),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::Transport(error) => write!(f, "{error}"),
            Refused::Status(status) => write!(f, "refused with HTTP {status}"),
        }
    }
}

/// The waits between a delivery's `attempts`: [`retry_base`] (500 ms),
/// doubling, at most [`RETRY_MAX`]: 0.5, 1, 2, 4 and 8 s for the default
/// six attempts.
fn backoff(attempts: u32) -> ExponentialBuilder {
    ExponentialBuilder::default()
        .with_min_delay(retry_base())
        .with_factor(2.0)
        .with_max_delay(RETRY_MAX)
        .with_max_times(attempts.saturating_sub(1) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use branchyard::CandidateInfo;

    /// The waits are the hand-written formula's from before backon:
    /// `min(base * 2^(attempt-1), RETRY_MAX)` after each failed attempt but
    /// the last.
    #[test]
    fn delivery_backoff_keeps_the_old_timing() {
        use backon::BackoffBuilder;
        for attempts in [0u32, 1, 2, 6, 12] {
            let waits: Vec<Duration> = backoff(attempts).build().collect();
            let old: Vec<Duration> = (1..attempts.max(1))
                .map(|attempt| {
                    retry_base()
                        .saturating_mul(1u32 << (attempt - 1).min(16))
                        .min(RETRY_MAX)
                })
                .collect();
            assert_eq!(waits, old, "{attempts} attempts");
        }
    }

    #[test]
    fn kinds_tag_status_stall_and_permission_requests() {
        assert_eq!(kinds_of(&Activity::Status(BranchStatus::Ready)), ["status"]);
        assert_eq!(
            kinds_of(&Activity::Status(BranchStatus::Merged {
                target: "main".into(),
                commit: "c".into(),
            })),
            ["status", "merge"]
        );
        assert_eq!(
            kinds_of(&Activity::Status(BranchStatus::Failed {
                reason: "x".into()
            })),
            ["status", "failure"]
        );
        assert_eq!(kinds_of(&Activity::Stalled { since_ms: 1 }), ["stall"]);
        assert_eq!(
            kinds_of(&Activity::Harness(branchyard::Event::PermissionRequested {
                turn: Some(1),
                request: branchyard::PermissionRequest {
                    key: branchyard::PermissionKey("k".into()),
                    tool: "t".into(),
                    input: serde_json::Value::Null,
                },
            })),
            ["permission_wait"]
        );
        assert!(kinds_of(&Activity::Warning("x".into())).is_empty());
        // Sanity: a snapshot (no kind) is not accidentally matched.
        assert!(kinds_of(&Activity::Snapshot(CandidateInfo {
            commit: "c".into(),
            files_changed: 1,
            insertions: 1,
            deletions: 0,
        }))
        .is_empty());
    }

    fn webhook(events: &[&str]) -> WebhookConfig {
        WebhookConfig {
            id: "h".into(),
            url: "https://example.invalid/hook".into(),
            secret: "0123456789abcdef".into(),
            events: events.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn an_empty_filter_wants_everything_and_others_narrow() {
        assert!(wants(&webhook(&[]), &["status"]));
        assert!(wants(&webhook(&[]), &[]));
        assert!(wants(&webhook(&["stall"]), &["stall"]));
        assert!(!wants(&webhook(&["stall"]), &["status"]));
        assert!(wants(&webhook(&["stall", "merge"]), &["status", "merge"]));
    }

    #[test]
    fn signatures_are_deterministic_and_key_dependent() {
        let a = sign("secret-one", b"body");
        let b = sign("secret-one", b"body");
        let c = sign("secret-two", b"body");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64, "hex-encoded SHA-256");
    }
}
