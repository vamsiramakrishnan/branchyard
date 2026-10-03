//! `/v1/triggers`: create, list, show, enable, disable, test and remove
//! triggers, read their runs, set a webhook secret; and
//! `POST /v1/triggers/{id}/fire`, where webhook senders deliver, which is
//! authenticated by the trigger's own signature instead of a bearer token
//! (and `POST /v1/triggers/{id}/fire/{token}`, for an inbound-mail provider
//! that signs nothing and cannot send a password: the token is the
//! trigger's secret).
//!
//! A trigger belongs to the tenant of the principal that created it and is
//! visible only to that tenant's principals who may reach its repository;
//! another's reads as `404 unknown_trigger`. Reads need `read`; every
//! change needs `run` on the trigger's repository.

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use branchyard_client::triggers::{
    FireAck, RunState, SecretRequest, SecretSet, TriggerCreated, TriggerList, TriggerRemoved,
    TriggerRun, TriggerRuns, TriggerSpec, TriggerTest, TriggerTestRequest, TriggerToggle, When,
};

use super::email;
use super::events::{self, Delivery};
use super::store::Recorded;
use super::{engine::Engine, Schedule, StoredTrigger};
use crate::api::{blocking, Caller, JsonBody, Shared};
use crate::error::ApiError;

pub(crate) fn router() -> Router<Shared> {
    Router::new()
        .route("/v1/triggers", get(list).post(create))
        .route("/v1/triggers/{t}", get(show).delete(remove))
        .route("/v1/triggers/{t}/enable", post(enable))
        .route("/v1/triggers/{t}/disable", post(disable))
        .route("/v1/triggers/{t}/secret", post(secret))
        .route("/v1/triggers/{t}/test", post(test))
        .route("/v1/triggers/{t}/runs", get(runs))
        .route("/v1/triggers/{t}/fire", post(fire))
        .route("/v1/triggers/{t}/fire/{token}", post(fire_with_token))
}

/// Whether `path` is a trigger's webhook endpoint, which takes no bearer
/// token: `/v1/triggers/{id}/fire`, or with one more segment.
pub fn is_fire(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/v1/triggers/") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        [id, "fire"] => !id.is_empty(),
        [id, "fire", token] => !id.is_empty() && !token.is_empty(),
        _ => false,
    }
}

fn unknown(key: &str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "unknown_trigger",
        format!("no trigger named {key}"),
    )
}

fn store_error(e: std::io::Error) -> ApiError {
    ApiError::internal(format!("trigger store: {e}"))
}

/// The trigger `key` (name or ID) of `caller`'s tenant, on a repository
/// the caller may see; others read as unknown.
async fn find(app: &Shared, caller: &Caller, key: &str) -> Result<StoredTrigger, ApiError> {
    let store = app.triggers.store.clone();
    let (tenant, k) = (caller.tenant().to_owned(), key.to_owned());
    let found = blocking(move || store.find(Some(&tenant), &k))
        .await?
        .map_err(store_error)?;
    let policy = app.tenant_policy(caller);
    let visible: Vec<StoredTrigger> = found
        .into_iter()
        .filter(|t| caller.0.repo_allowed(&policy, &t.spec.repo))
        .collect();
    match visible.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(unknown(key)),
        // A name and another's ID coinciding: the ID wins.
        several => several
            .iter()
            .find(|t| t.id == key)
            .cloned()
            .ok_or_else(|| unknown(key)),
    }
}

/// `find`, then `run` on its repository.
async fn find_for_change(
    app: &Shared,
    caller: &Caller,
    key: &str,
) -> Result<StoredTrigger, ApiError> {
    let trigger = find(app, caller, key).await?;
    app.authorized_repo(caller, &trigger.spec.repo, "run")?;
    Ok(trigger)
}

fn info(app: &Shared, trigger: &StoredTrigger) -> branchyard_client::triggers::Trigger {
    trigger.info(&app.triggers.base_url)
}

/// When an enabled schedule next fires, counted from `now`.
fn next_due(when: &When, now: u64) -> Result<Option<u64>, ApiError> {
    Ok(Schedule::of(when)
        .map_err(ApiError::bad_request)?
        .and_then(|s| s.next_after(now, now)))
}

async fn create(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    JsonBody(spec, _): JsonBody<TriggerSpec>,
) -> Result<Response, ApiError> {
    let repo = app.authorized_repo(&caller, &spec.repo, "run")?.clone();
    let mut spec = super::validate(spec).map_err(ApiError::bad_request)?;
    if spec.precheck.is_some() && !app.config.triggers.allow_prechecks.allows(&repo.name) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "precheck_not_allowed",
            format!(
                "this server does not let {}'s triggers run prechecks (allow_trigger_prechecks)",
                repo.name
            ),
        ));
    }
    // What the server would refuse of the task itself (a provider,
    // delegation, a command, secrets), refused now rather than at every
    // firing.
    crate::work::task_options(&app, &repo, &spec.task)?;
    if let Some(bad) = spec
        .task
        .require_labels
        .iter()
        .find(|l| !crate::store::valid_label(l))
    {
        return Err(ApiError::bad_request(format!(
            "task.require_labels: {bad:?} is not a label"
        )));
    }
    let now = app.triggers.settings.clock.now();
    let event = !spec.when.is_schedule();
    let (secret, generated) = match (spec.secret.take(), event) {
        (Some(secret), _) => (Some(secret), None),
        (None, true) => {
            let secret = super::new_secret();
            (Some(secret.clone()), Some(secret))
        }
        (None, false) => (None, None),
    };
    let enabled = spec.enabled;
    let trigger = StoredTrigger {
        id: super::new_id("trg"),
        tenant: caller.tenant().to_owned(),
        next_due_ms: match enabled {
            true => next_due(&spec.when, now)?,
            false => None,
        },
        spec,
        principal: caller.0.clone(),
        created_at_ms: now,
        secret,
        enabled,
        paused_reason: (!enabled).then(|| format!("created disabled by {}", caller.name())),
        failures: 0,
    };
    let store = app.triggers.store.clone();
    let stored = trigger.clone();
    let created = blocking(move || store.create(&stored))
        .await?
        .map_err(store_error)?;
    if !created {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "trigger_exists",
            format!("a trigger named {} exists already", trigger.spec.name),
        )
        .detail(serde_json::json!({ "name": trigger.spec.name })));
    }
    tracing::info!(trigger = %trigger.spec.name, id = %trigger.id, by = %caller.name(), "trigger created");
    Ok((
        StatusCode::CREATED,
        Json(TriggerCreated {
            trigger: info(&app, &trigger),
            secret: generated,
        }),
    )
        .into_response())
}

async fn list(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<TriggerList>, ApiError> {
    let store = app.triggers.store.clone();
    let tenant = caller.tenant().to_owned();
    let all = blocking(move || store.list(Some(&tenant)))
        .await?
        .map_err(store_error)?;
    let policy = app.tenant_policy(&caller);
    Ok(Json(TriggerList {
        triggers: all
            .iter()
            .filter(|t| caller.0.repo_allowed(&policy, &t.spec.repo))
            .map(|t| info(&app, t))
            .collect(),
    }))
}

async fn show(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path(key): Path<String>,
) -> Result<Json<branchyard_client::triggers::Trigger>, ApiError> {
    let trigger = find(&app, &caller, &key).await?;
    Ok(Json(info(&app, &trigger)))
}

async fn remove(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path(key): Path<String>,
) -> Result<Json<TriggerRemoved>, ApiError> {
    let trigger = find_for_change(&app, &caller, &key).await?;
    let store = app.triggers.store.clone();
    let id = trigger.id.clone();
    blocking(move || store.remove(&id))
        .await?
        .map_err(store_error)?;
    Ok(Json(TriggerRemoved {
        removed: trigger.spec.name,
    }))
}

async fn toggle(app: Shared, caller: Caller, key: String, on: bool) -> Result<Response, ApiError> {
    let trigger = find_for_change(&app, &caller, &key).await?;
    let now = app.triggers.settings.clock.now();
    let (next, reason, failures) = match on {
        true => (next_due(&trigger.spec.when, now)?, None, 0),
        false => (
            None,
            Some(format!("disabled by {}", caller.name())),
            trigger.failures,
        ),
    };
    let store = app.triggers.store.clone();
    let id = trigger.id.clone();
    let reason_text = reason.clone();
    blocking(move || store.set_state(&id, on, reason_text.as_deref(), failures, next))
        .await?
        .map_err(store_error)?;
    let store = app.triggers.store.clone();
    let id = trigger.id.clone();
    let updated = blocking(move || store.get(&id))
        .await?
        .map_err(store_error)?
        .ok_or_else(|| unknown(&key))?;
    if on {
        app.triggers.wake.notify_one();
    }
    Ok(Json(info(&app, &updated)).into_response())
}

async fn enable(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path(key): Path<String>,
    JsonBody(_, _): JsonBody<TriggerToggle>,
) -> Result<Response, ApiError> {
    toggle(app, caller, key, true).await
}

async fn disable(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path(key): Path<String>,
    JsonBody(_, _): JsonBody<TriggerToggle>,
) -> Result<Response, ApiError> {
    toggle(app, caller, key, false).await
}

async fn secret(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path(key): Path<String>,
    JsonBody(request, _): JsonBody<SecretRequest>,
) -> Result<Json<SecretSet>, ApiError> {
    let trigger = find_for_change(&app, &caller, &key).await?;
    if trigger.spec.when.is_schedule() {
        return Err(ApiError::bad_request("a schedule takes no webhook secret"));
    }
    let (secret, generated) = match request.secret {
        Some(s) if s.trim().is_empty() || s.contains(['\r', '\n']) => {
            return Err(ApiError::bad_request("the secret is empty or spans lines"))
        }
        Some(s) => (s, None),
        None => {
            let s = super::new_secret();
            (s.clone(), Some(s))
        }
    };
    let store = app.triggers.store.clone();
    blocking(move || store.set_secret(&trigger.id, &secret))
        .await?
        .map_err(store_error)?;
    Ok(Json(SecretSet { secret: generated }))
}

fn query_limit(query: Option<&str>) -> Result<usize, ApiError> {
    for pair in query.unwrap_or("").split('&') {
        if let Some(value) = pair.strip_prefix("limit=") {
            return value
                .parse::<usize>()
                .map(|n| n.clamp(1, 200))
                .map_err(|_| ApiError::bad_request("limit must be a whole number"));
        }
    }
    Ok(20)
}

async fn runs(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path(key): Path<String>,
    RawQuery(query): RawQuery,
) -> Result<Json<TriggerRuns>, ApiError> {
    let limit = query_limit(query.as_deref())?;
    let trigger = find(&app, &caller, &key).await?;
    let store = app.triggers.store.clone();
    let runs = blocking(move || store.runs(&trigger.id, limit))
        .await?
        .map_err(store_error)?;
    Ok(Json(TriggerRuns { runs }))
}

/// What `trigger` would do with `request`'s event (or at its next
/// scheduled time), creating nothing; with `run_precheck` and a precheck,
/// in `scratch`, with `root` the repository.
pub fn evaluate(
    trigger: &StoredTrigger,
    request: &TriggerTestRequest,
    now: u64,
    precheck: Option<(&std::path::Path, &std::path::Path)>,
) -> Result<TriggerTest, String> {
    let mut run = TriggerRun {
        id: "run_test".into(),
        trigger: trigger.id.clone(),
        key: String::new(),
        state: RunState::Pending,
        at_ms: now,
        scheduled_ms: None,
        missed: None,
        last_missed_ms: None,
        event: None,
        reason: None,
        precheck: None,
        operation: None,
        branches: Vec::new(),
        outcome: None,
        finished_at_ms: None,
    };
    let mut test = TriggerTest {
        event: None,
        matched: true,
        reason: None,
        precheck: None,
        would_fire: false,
        task: None,
        route: trigger.spec.route.clone(),
        key: String::new(),
    };
    match trigger.source() {
        Some(source) => {
            let Some(body) = &request.event else {
                return Err("give an event to test an event trigger with".into());
            };
            let bytes = serde_json::to_vec(body).map_err(|e| e.to_string())?;
            match events::read(source, request.event_type.as_deref(), None, body, &bytes)? {
                Delivery::Challenge(_) => {
                    test.matched = false;
                    test.reason = Some("a URL verification request fires nothing".into());
                    return Ok(test);
                }
                Delivery::Ignored(why) => {
                    test.matched = false;
                    test.reason = Some(why);
                    return Ok(test);
                }
                Delivery::Event(event) => {
                    run.key = format!("event:{}", event.id);
                    if let Err(why) = super::matches(&trigger.spec.conditions, &event) {
                        test.matched = false;
                        test.reason = Some(format!("the conditions do not match: {why}"));
                    }
                    run.event = Some(*event);
                }
            }
        }
        None => {
            if request.event.is_some() {
                return Err("a schedule has no event to test with".into());
            }
            let at = trigger
                .next_due_ms
                .or_else(|| {
                    Schedule::of(&trigger.spec.when)
                        .ok()
                        .flatten()
                        .and_then(|s| s.next_after(now, now))
                })
                .unwrap_or(now);
            run.scheduled_ms = Some(at);
            run.key = format!("schedule:{at}");
        }
    }
    test.key = run.key.clone();
    test.event = run.event.clone();
    test.task = Some(Engine::render(trigger, &run)?);
    if !test.matched {
        return Ok(test);
    }
    if let (Some(check), Some((scratch, root))) = (&trigger.spec.precheck, precheck) {
        let result = super::engine::run_precheck(scratch, root, trigger, &run, check)?;
        if !result.passed() {
            test.reason = Some(super::precheck::failure(&result));
            test.precheck = Some(result);
            return Ok(test);
        }
        test.precheck = Some(result);
    }
    if !trigger.enabled {
        test.reason = Some(match &trigger.paused_reason {
            Some(why) => format!("the trigger is disabled: {why}"),
            None => "the trigger is disabled".into(),
        });
        return Ok(test);
    }
    test.would_fire = true;
    Ok(test)
}

async fn test(
    State(app): State<Shared>,
    Extension(caller): Extension<Caller>,
    Path(key): Path<String>,
    JsonBody(request, _): JsonBody<TriggerTestRequest>,
) -> Result<Json<TriggerTest>, ApiError> {
    let trigger = find_for_change(&app, &caller, &key).await?;
    let precheck = match (request.run_precheck, &trigger.spec.precheck) {
        (true, Some(_)) => {
            if !app
                .config
                .triggers
                .allow_prechecks
                .allows(&trigger.spec.repo)
            {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "precheck_not_allowed",
                    format!(
                        "this server does not let {}'s triggers run prechecks \
                         (allow_trigger_prechecks)",
                        trigger.spec.repo
                    ),
                ));
            }
            let repo = app.repo(&trigger.spec.repo)?;
            Some((
                app.config.data_dir.join("triggers"),
                repo.yard.root().to_path_buf(),
            ))
        }
        _ => None,
    };
    let now = app.triggers.settings.clock.now();
    let tested = blocking(move || {
        evaluate(
            &trigger,
            &request,
            now,
            precheck.as_ref().map(|(s, r)| (s.as_path(), r.as_path())),
        )
    })
    .await?
    .map_err(ApiError::bad_request)?;
    Ok(Json(tested))
}

fn refused(code: &str, message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::UNAUTHORIZED, code, message)
}

/// `POST /v1/triggers/{id}/fire`: a webhook delivery, verified with the
/// trigger's secret, recorded as a run once per event ID.
async fn fire(
    State(app): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    deliver(app, id, None, headers, body).await
}

/// `POST /v1/triggers/{id}/fire/{token}`: the same, for an email source
/// whose URL carries the secret (or `json`, Mailgun's JSON form).
async fn fire_with_token(
    State(app): State<Shared>,
    Path((id, token)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    deliver(app, id, Some(token), headers, body).await
}

async fn deliver(
    app: Shared,
    id: String,
    token: Option<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    if body.len() > app.config.max_body_bytes {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            format!(
                "the delivery is larger than {} bytes",
                app.config.max_body_bytes
            ),
        ));
    }
    let store = app.triggers.store.clone();
    let key = id.clone();
    let trigger = blocking(move || store.get(&key))
        .await?
        .map_err(store_error)?;
    // A schedule, or no trigger at all: the same answer, so IDs cannot be
    // probed for what they are.
    let Some(trigger) = trigger.filter(|t| !t.spec.when.is_schedule()) else {
        return Err(unknown(&id));
    };
    let source = trigger.source().expect("an event trigger");
    let Some(secret) = trigger.secret.clone() else {
        return Err(refused(
            "invalid_signature",
            "this trigger has no secret yet; set one with by trigger secret",
        ));
    };
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let now = app.triggers.settings.clock.now();
    let window = trigger.spec.policy.replay_window_seconds;
    let answer = |r: events::Refused| match r.code {
        "invalid_request" => ApiError::bad_request(r.message),
        code => refused(code, r.message),
    };
    let (delivery, nonce) = match (source.is_email(), &token) {
        (true, _) => {
            let received = email::receive(
                source,
                &header,
                &body,
                &secret,
                token.as_deref(),
                now,
                window,
            )
            .map_err(answer)?;
            (received.delivery, received.nonce)
        }
        // Only an email source's URL has a segment after /fire.
        (false, Some(_)) => return Err(unknown(&id)),
        (false, None) => (
            events::receive(source, &header, &body, &secret, now, window).map_err(answer)?,
            None,
        ),
    };
    let event = match delivery {
        Delivery::Challenge(challenge) => {
            return Ok(Json(serde_json::json!({ "challenge": challenge })).into_response())
        }
        Delivery::Ignored(why) => {
            return Ok(Json(FireAck {
                ignored: Some(why),
                ..FireAck::default()
            })
            .into_response())
        }
        Delivery::Event(event) => *event,
    };
    // An email from a sender the trigger does not allow is recorded
    // nowhere. It is answered 200, not refused, so that the provider does
    // not retry it for days.
    if let Err(why) = super::sender_allowed(&trigger.spec.conditions, &event) {
        return Ok(Json(FireAck {
            ignored: Some(why),
            ..FireAck::default()
        })
        .into_response());
    }
    if !trigger.enabled {
        return Ok(Json(FireAck {
            ignored: Some(match &trigger.paused_reason {
                Some(why) => format!("the trigger is disabled: {why}"),
                None => "the trigger is disabled".into(),
            }),
            ..FireAck::default()
        })
        .into_response());
    }
    let mut run = TriggerRun {
        id: super::new_id("run"),
        trigger: trigger.id.clone(),
        key: format!("event:{}", event.id),
        state: RunState::Pending,
        at_ms: now,
        scheduled_ms: None,
        missed: None,
        last_missed_ms: None,
        event: Some(event),
        reason: None,
        precheck: None,
        operation: None,
        branches: Vec::new(),
        outcome: None,
        finished_at_ms: None,
    };
    // A one-time token (Mailgun's form `token`) is spent for this event:
    // carrying another body, it is a replay.
    if let Some(nonce) = nonce {
        let store = app.triggers.store.clone();
        let (trigger_id, key) = (trigger.id.clone(), run.key.clone());
        let expires = now.saturating_add(window.saturating_mul(2000));
        let first = blocking(move || store.claim_nonce(&trigger_id, &nonce, &key, now, expires))
            .await?
            .map_err(store_error)?;
        if first != run.key {
            return Err(refused(
                "stale_delivery",
                "this delivery's token was already used for another message",
            ));
        }
    }
    if let Err(why) = super::matches(&trigger.spec.conditions, run.event.as_ref().expect("set")) {
        run.state = RunState::SkippedCondition;
        run.reason = Some(format!("the conditions do not match: {why}"));
        run.finished_at_ms = Some(now);
    }
    let store = app.triggers.store.clone();
    let recorded = run.clone();
    let outcome = blocking(move || store.record(&recorded))
        .await?
        .map_err(store_error)?;
    match outcome {
        Recorded::Existing(existing) => Ok(Json(FireAck {
            run: Some(*existing),
            duplicate: true,
            ignored: None,
        })
        .into_response()),
        Recorded::Inserted => {
            let status = match run.state {
                RunState::Pending => {
                    app.triggers.wake.notify_one();
                    StatusCode::ACCEPTED
                }
                _ => StatusCode::OK,
            };
            Ok((
                status,
                Json(FireAck {
                    run: Some(run),
                    duplicate: false,
                    ignored: None,
                }),
            )
                .into_response())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_triggers_fire_path_skips_the_token() {
        assert!(is_fire("/v1/triggers/trg_abc/fire"));
        assert!(!is_fire("/v1/triggers//fire"));
        assert!(!is_fire("/v1/triggers/a/b/fire"));
        assert!(!is_fire("/v1/triggers/trg_abc"));
        assert!(is_fire("/v1/triggers/trg_abc/fire/x"));
        assert!(!is_fire("/v1/triggers/trg_abc/fire/"));
        assert!(!is_fire("/v1/triggers/trg_abc/fire/x/y"));
        assert!(!is_fire("/v1/repos/app/fire"));
    }
}
