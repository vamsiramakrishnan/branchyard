//! Triggers through a real server on loopback, with the fake ACP agent and
//! a manual clock: schedules fire once when the clock reaches them, signed
//! webhooks from GitHub, Slack and generic senders become runs once per
//! event, failures pause a trigger, prechecks skip runs, and a run recorded
//! by a server that stopped fires after the restart; inbound email from
//! Mailgun and Postmark is authenticated, allowlisted and fired once. With
//! the `postgres` feature and `BY_TEST_POSTGRES_URL`, the store's
//! conformance suite, the email deliveries on PostgreSQL, and two servers
//! on one database firing a schedule time once.

#![allow(clippy::expect_used, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use branchyard_client::api::{OperationKind, OperationState, PolicySpec, SendRequest, TaskRequest};
use branchyard_client::new_key;
use branchyard_client::triggers::{
    Busy, Conditions, Deliver, EventSource, Precheck, RunState, TriggerPolicy, TriggerRun,
    TriggerSpec, TriggerTestRequest, When,
};
use branchyard_client::Client;
use branchyard_server::config::WorkspaceScripts;
use branchyard_server::triggers::events::sign;
use branchyard_server::triggers::Clock;
use branchyard_server::Config;
use branchyard_testkit::wait;
use common::{await_operation, post, raw, run, task, Fixture, Server};

/// 2026-09-21T14:13:20Z.
const T0: u64 = 1_790_000_000_000;

fn with_clock(config: &mut Config) -> Arc<AtomicU64> {
    let time = Arc::new(AtomicU64::new(T0));
    config.triggers.clock = Clock::manual(time.clone());
    config.triggers.tick = Duration::from_millis(50);
    time
}

fn spec(name: &str, when: When, prompt: &str) -> TriggerSpec {
    TriggerSpec {
        name: name.into(),
        repo: "app".into(),
        when,
        conditions: Conditions::default(),
        task: TaskRequest {
            prompt: prompt.into(),
            harness: Some("gemini-cli".into()),
            policy: PolicySpec::allow_all(),
            ..TaskRequest::default()
        },
        route: None,
        deliver: None,
        precheck: None,
        enabled: true,
        policy: TriggerPolicy::default(),
        secret: None,
    }
}

fn event(source: EventSource) -> When {
    When::Event { source }
}

/// The trigger's runs once `done` holds for them.
fn runs_when(
    client: &Client,
    trigger: &str,
    what: &str,
    done: impl Fn(&[TriggerRun]) -> bool,
) -> Vec<TriggerRun> {
    let mut runs = Vec::new();
    wait::until(what, || {
        runs = client.trigger_runs(trigger, 50).unwrap();
        done(&runs)
    });
    runs
}

/// A webhook delivery to `id`, with `headers` (one per line, CRLF-ended).
fn deliver(server: &Server, id: &str, headers: &str, body: &str) -> (u16, serde_json::Value) {
    let (status, _, text) = raw(
        server.addr,
        &post(&format!("/v1/triggers/{id}/fire"), None, headers, body),
    );
    let value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    (status, value)
}

fn github_headers(secret: &str, kind: &str, delivery: &str, body: &str) -> String {
    format!(
        "X-GitHub-Event: {kind}\r\nX-GitHub-Delivery: {delivery}\r\n\
         X-Hub-Signature-256: sha256={}\r\n",
        sign(secret, &[body.as_bytes()])
    )
}

fn generic_headers(secret: &str, body: &str) -> String {
    format!(
        "X-Branchyard-Signature: sha256={}\r\n",
        sign(secret, &[body.as_bytes()])
    )
}

fn issue(number: u32, title: &str, labels: &[&str]) -> String {
    serde_json::json!({
        "action": "opened",
        "issue": {"number": number, "title": title, "body": "details",
                  "html_url": format!("https://github.com/acme/app/issues/{number}"),
                  "labels": labels.iter().map(|l| serde_json::json!({"name": l})).collect::<Vec<_>>()},
        "repository": {"full_name": "acme/app"},
        "sender": {"login": "alice"}
    })
    .to_string()
}

#[test]
fn a_schedule_fires_once_when_the_clock_reaches_it_and_its_outcome_is_recorded() {
    let f = Fixture::new();
    let mut config = f.config();
    let clock = with_clock(&mut config);
    let server = Server::start(config);
    let client = server.client();
    let created = client
        .create_trigger(
            &spec(
                "nightly",
                When::Interval { seconds: 3600 },
                "WRITE nightly.txt=done at {{scheduled_at}}",
            ),
            &new_key(),
        )
        .unwrap();
    assert_eq!(created.secret, None, "a schedule has no secret");
    let trigger = created.trigger;
    assert_eq!(trigger.next_due_ms, Some(T0 + 3_600_000));
    assert_eq!(trigger.webhook_url, None);
    // Nothing before its time, however often the dispatcher looks.
    wait::settle(
        "the dispatcher looks often and still finds nothing due",
        Duration::from_millis(300),
    );
    assert!(client.trigger_runs("nightly", 10).unwrap().is_empty());

    clock.store(T0 + 3_600_000 + 5_000, Ordering::SeqCst);
    let runs = runs_when(&client, "nightly", "the scheduled run to fire", |runs| {
        runs.iter().any(|r| r.state == RunState::Fired)
    });
    assert_eq!(runs.len(), 1, "{runs:?}");
    let fired = &runs[0];
    assert_eq!(fired.key, format!("schedule:{}", T0 + 3_600_000));
    assert_eq!(fired.branches, ["nightly-20260921-1513"]);
    let op = await_operation(&client, fired.operation.as_ref().unwrap());
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    let runs = runs_when(&client, "nightly", "the run to settle", |runs| {
        runs[0].outcome.is_some()
    });
    assert!(runs[0].outcome.as_ref().unwrap().ok, "{:?}", runs[0]);
    let shown = client.trigger("nightly").unwrap();
    assert_eq!(shown.next_due_ms, Some(T0 + 7_200_000));
    // Still once, after more ticks at the same time.
    wait::settle("more ticks at the same time", Duration::from_millis(300));
    assert_eq!(client.trigger_runs("nightly", 10).unwrap().len(), 1);
    let prompt = client.repo("app").branch("nightly-20260921-1513").unwrap();
    assert_eq!(prompt.name, "nightly-20260921-1513");
}

/// A trigger that delivers to a branch keeps one branch alive run after
/// run, as a chat assistant keeps a session: the first run creates it from
/// the task, later runs continue it, and while it runs a turn the
/// trigger's busy policy decides: skip (a heartbeat), steer, or queue.
#[test]
fn a_trigger_that_delivers_to_a_branch_continues_it_run_after_run() {
    let f = Fixture::new();
    let mut config = f.config();
    let clock = with_clock(&mut config);
    let server = Server::start(config);
    let client = server.client();
    let app = client.repo("app");
    let mut s = spec(
        "heartbeat",
        When::Interval { seconds: 60 },
        "WRITE beat.txt={{scheduled_at}}",
    );
    s.deliver = Some(Deliver {
        branch: "assistant".into(),
        busy: Busy::Skip,
    });
    client.create_trigger(&s, &new_key()).unwrap();
    // The first run creates the branch, under the delivered name.
    clock.store(T0 + 60_000 + 5_000, Ordering::SeqCst);
    let runs = runs_when(&client, "heartbeat", "the first run to fire", |runs| {
        runs.iter().any(|r| r.state == RunState::Fired)
    });
    assert_eq!(runs[0].branches, ["assistant"], "{runs:?}");
    let op = await_operation(&client, runs[0].operation.as_ref().unwrap());
    assert_eq!(
        (op.kind, op.state),
        (OperationKind::Task, OperationState::Succeeded),
        "{op:?}"
    );
    assert_eq!(app.branch("assistant").unwrap().turns, 1);
    // The second continues it: a send, one more turn, no second branch.
    clock.store(T0 + 120_000 + 5_000, Ordering::SeqCst);
    let runs = runs_when(&client, "heartbeat", "the second run to fire", |runs| {
        runs.len() == 2 && runs[0].state == RunState::Fired
    });
    assert_eq!(runs[0].branches, ["assistant"]);
    let op = await_operation(&client, runs[0].operation.as_ref().unwrap());
    assert_eq!(
        (op.kind, op.state),
        (OperationKind::Send, OperationState::Succeeded),
        "{op:?}"
    );
    assert_eq!(app.branch("assistant").unwrap().turns, 2);
    assert_eq!(app.branches().unwrap().len(), 1);
    let runs = runs_when(&client, "heartbeat", "both runs to settle", |runs| {
        runs.iter().all(|r| r.outcome.is_some())
    });
    assert!(
        runs.iter().all(|r| r.outcome.as_ref().unwrap().ok),
        "{runs:?}"
    );

    // A turn that waits to be steered keeps the branch running.
    let waiting = |prompt: &str| {
        let op = app
            .send(
                "assistant",
                &SendRequest {
                    prompt: prompt.into(),
                    policy: PolicySpec::allow_all(),
                    ..SendRequest::default()
                },
                &new_key(),
            )
            .unwrap();
        wait::until("the turn to run", || {
            app.branch("assistant").unwrap().status == branchyard::BranchStatus::Running
        });
        op
    };
    let held = waiting("AWAIT_STEER");
    // Skip: the run is recorded, fires nothing, and is no failure.
    clock.store(T0 + 180_000 + 5_000, Ordering::SeqCst);
    let runs = runs_when(
        &client,
        "heartbeat",
        "the third run to be skipped",
        |runs| runs.len() == 3 && runs[0].state != RunState::Pending,
    );
    assert_eq!(runs[0].state, RunState::SkippedBusy, "{runs:?}");
    assert!(runs[0]
        .reason
        .as_deref()
        .unwrap()
        .contains("running a turn"));
    assert_eq!(runs[0].branches, ["assistant"]);
    assert_eq!(client.trigger("heartbeat").unwrap().consecutive_failures, 0);
    client.disable_trigger("heartbeat", &new_key()).unwrap();

    // Steer: the prompt joins the running turn, which ends on it (the fake
    // agent's AWAIT_STEER); the run is settled as it fires.
    let mut s = spec("nudge", When::Interval { seconds: 60 }, "carry on");
    s.deliver = Some(Deliver {
        branch: "assistant".into(),
        busy: Busy::Steer,
    });
    client.create_trigger(&s, &new_key()).unwrap();
    clock.store(T0 + 240_000 + 10_000, Ordering::SeqCst);
    let runs = runs_when(&client, "nudge", "the steering run to fire", |runs| {
        runs.iter().any(|r| r.state == RunState::Fired)
    });
    assert!(runs[0].operation.is_none(), "{runs:?}");
    let outcome = runs[0].outcome.as_ref().unwrap();
    assert!(
        outcome.ok && outcome.detail.contains("steered"),
        "{outcome:?}"
    );
    let op = await_operation(&client, &held.id);
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    assert_eq!(
        app.branch("assistant").unwrap().turns,
        3,
        "a steer adds no turn"
    );
    client.disable_trigger("nudge", &new_key()).unwrap();

    // Queue: the send waits for the turn, then runs as the next one.
    let mut s = spec(
        "followup",
        When::Interval { seconds: 60 },
        "WRITE later.txt=1",
    );
    s.deliver = Some(Deliver {
        branch: "assistant".into(),
        busy: Busy::Queue,
    });
    client.create_trigger(&s, &new_key()).unwrap();
    let held = waiting("AWAIT_STEER");
    clock.store(T0 + 300_000 + 15_000, Ordering::SeqCst);
    // The run waits, pending and saying so, while the turn runs.
    let runs = runs_when(
        &client,
        "followup",
        "the run to wait for the turn",
        |runs| runs.iter().any(|r| r.reason.is_some()),
    );
    assert_eq!(runs[0].state, RunState::Pending, "{runs:?}");
    assert!(runs[0].reason.as_deref().unwrap().contains("waits for it"));
    assert_eq!(runs[0].operation, None);
    assert_eq!(runs[0].branches, ["assistant"]);
    app.steer("assistant", "go on").unwrap();
    assert_eq!(
        await_operation(&client, &held.id).state,
        OperationState::Succeeded
    );
    // Looked at again once the deferral passes, it sends.
    clock.store(T0 + 300_000 + 15_000 + 31_000, Ordering::SeqCst);
    let runs = runs_when(&client, "followup", "the waiting run to send", |runs| {
        runs.iter().any(|r| r.state == RunState::Fired)
    });
    let sent = runs.iter().find(|r| r.state == RunState::Fired).unwrap();
    let op = await_operation(&client, sent.operation.as_ref().unwrap());
    assert_eq!(
        (op.kind, op.state),
        (OperationKind::Send, OperationState::Succeeded),
        "{op:?}"
    );
    assert_eq!(app.branch("assistant").unwrap().turns, 5);
    assert_eq!(app.branches().unwrap().len(), 1, "still one branch");
    // `by trigger test` shows the branch it would continue.
    let test = client
        .test_trigger("followup", &TriggerTestRequest::default(), &new_key())
        .unwrap();
    assert_eq!(
        test.deliver,
        Some(Deliver {
            branch: "assistant".into(),
            busy: Busy::Queue
        })
    );
    assert_eq!(test.task.unwrap().name.as_deref(), Some("assistant"));
}

#[test]
fn github_deliveries_are_verified_matched_rendered_and_never_fired_twice() {
    let f = Fixture::new();
    let mut config = f.config();
    with_clock(&mut config);
    config.triggers.public_url = Some("https://by.example.com/".into());
    let server = Server::start(config);
    let client = server.client();
    let mut s = spec(
        "issues",
        event(EventSource::Github),
        "WRITE issue-{{event.number}}.txt={{event.title}}",
    );
    s.secret = Some("gh-secret".into());
    s.conditions = Conditions {
        kind: vec!["issues.*".into()],
        label: vec!["agent".into()],
        ..Conditions::default()
    };
    s.task.name = Some("fix-{{event.number}}".into());
    let created = client.create_trigger(&s, &new_key()).unwrap();
    assert_eq!(created.secret, None, "the given secret is not echoed");
    let id = created.trigger.id.clone();
    assert_eq!(
        created.trigger.webhook_url.as_deref(),
        Some(format!("https://by.example.com/v1/triggers/{id}/fire").as_str())
    );

    let body = issue(42, "Parser-crash", &["bug", "agent"]);
    let (status, ack) = deliver(
        &server,
        &id,
        &github_headers("gh-secret", "issues", "d-1", &body),
        &body,
    );
    assert_eq!(status, 202, "{ack}");
    assert_eq!(ack["run"]["state"], "pending");
    let runs = runs_when(&client, "issues", "the delivery to fire", |runs| {
        runs.iter().any(|r| r.state == RunState::Fired)
    });
    let fired = &runs[0];
    assert_eq!(fired.branches, ["fix-42"]);
    assert_eq!(fired.event.as_ref().unwrap().number.as_deref(), Some("42"));
    let op = await_operation(&client, fired.operation.as_ref().unwrap());
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    let diff = client.repo("app").diff("fix-42").unwrap();
    assert!(diff.contains("issue-42.txt"), "{diff}");
    assert!(diff.contains("Parser-crash"), "{diff}");

    // GitHub redelivers the same body: the same run. So is a captured
    // delivery replayed under a new (unsigned) delivery ID.
    for delivery in ["d-1", "d-1-replayed"] {
        let (status, ack) = deliver(
            &server,
            &id,
            &github_headers("gh-secret", "issues", delivery, &body),
            &body,
        );
        assert_eq!(status, 200, "{ack}");
        assert_eq!(ack["duplicate"], true);
        assert_eq!(ack["run"]["id"], serde_json::json!(fired.id));
    }
    assert_eq!(
        fired.event.as_ref().unwrap().delivery.as_deref(),
        Some("d-1")
    );
    // A wrong signature, a missing one, or a tampered body: 401.
    let (status, ack) = deliver(
        &server,
        &id,
        &github_headers("not-it", "issues", "d-2", &body),
        &body,
    );
    assert_eq!(
        (status, ack["error"]["code"].as_str()),
        (401, Some("invalid_signature"))
    );
    let (status, _) = deliver(&server, &id, "X-GitHub-Event: issues\r\n", &body);
    assert_eq!(status, 401);
    let tampered = body.replace("Parser-crash", "Something-else");
    let (status, _) = deliver(
        &server,
        &id,
        &github_headers("gh-secret", "issues", "d-3", &body),
        &tampered,
    );
    assert_eq!(status, 401);
    // Unlabeled: recorded as skipped by condition, nothing fired.
    let plain = issue(43, "Other", &["bug"]);
    let (status, ack) = deliver(
        &server,
        &id,
        &github_headers("gh-secret", "issues", "d-4", &plain),
        &plain,
    );
    assert_eq!(status, 200, "{ack}");
    assert_eq!(ack["run"]["state"], "skipped_condition");
    assert!(ack["run"]["reason"]
        .as_str()
        .unwrap()
        .contains("no label agent"));
    // A ping is answered and ignored.
    let ping = r#"{"zen":"Design for failure.","hook_id":1}"#;
    let (status, ack) = deliver(
        &server,
        &id,
        &github_headers("gh-secret", "ping", "d-5", ping),
        ping,
    );
    assert_eq!(status, 200);
    assert!(ack["ignored"].as_str().unwrap().contains("ping"));
    // Unknown IDs and schedules are not webhook endpoints.
    let (status, _) = deliver(
        &server,
        "trg_nope",
        &github_headers("gh-secret", "issues", "d-6", &body),
        &body,
    );
    assert_eq!(status, 404);
    let schedule = client
        .create_trigger(
            &spec("hourly", When::Interval { seconds: 3600 }, "x"),
            &new_key(),
        )
        .unwrap()
        .trigger;
    let (status, _) = deliver(
        &server,
        &schedule.id,
        &github_headers("gh-secret", "issues", "d-7", &body),
        &body,
    );
    assert_eq!(status, 404);
    // Exactly one task in all of that.
    let runs = client.trigger_runs("issues", 50).unwrap();
    assert_eq!(
        runs.iter().filter(|r| r.state == RunState::Fired).count(),
        1,
        "{runs:?}"
    );
    assert_eq!(runs.len(), 2, "the fired one and the skipped one");
}

#[test]
fn repeated_failures_pause_a_trigger_until_it_is_enabled() {
    let f = Fixture::new();
    let mut config = f.config();
    with_clock(&mut config);
    let server = Server::start(config);
    let client = server.client();
    let taken = run(&client, &task("WRITE t.txt=x", "taken"));
    assert_eq!(taken.state, OperationState::Succeeded);
    let mut s = spec(
        "deploys",
        event(EventSource::Generic),
        "WRITE d.txt={{event.text}}",
    );
    s.task.name = Some("taken".into());
    let created = client.create_trigger(&s, &new_key()).unwrap();
    let secret = created.secret.expect("a generated secret, shown once");
    let id = created.trigger.id;
    for n in 1..=3 {
        let body = format!(r#"{{"id":"e-{n}","kind":"deploy.failed","text":"boom"}}"#);
        let (status, ack) = deliver(&server, &id, &generic_headers(&secret, &body), &body);
        assert_eq!(status, 202, "{ack}");
        runs_when(&client, "deploys", "the run to fail", |runs| {
            runs.len() == n && runs.iter().all(|r| r.state == RunState::Failed)
        });
    }
    let paused = client.trigger("deploys").unwrap();
    assert!(!paused.enabled);
    assert_eq!(paused.consecutive_failures, 3);
    let reason = paused.paused_reason.unwrap();
    assert!(reason.contains("3 failed runs in a row"), "{reason}");
    assert!(reason.contains("taken"), "{reason}");
    let body = r#"{"id":"e-4","text":"again"}"#;
    let (status, ack) = deliver(&server, &id, &generic_headers(&secret, body), body);
    assert_eq!(status, 200);
    assert!(
        ack["ignored"].as_str().unwrap().contains("disabled"),
        "{ack}"
    );

    let enabled = client.enable_trigger("deploys", &new_key()).unwrap();
    assert!(enabled.enabled);
    assert_eq!(
        (enabled.consecutive_failures, enabled.paused_reason),
        (0, None)
    );
    let disabled = client.disable_trigger("deploys", &new_key()).unwrap();
    assert_eq!(
        disabled.paused_reason.as_deref(),
        Some("disabled by tester")
    );
    // A new secret, generated and shown once; the old one stops working.
    let rotated = client
        .set_trigger_secret("deploys", None, &new_key())
        .unwrap()
        .secret
        .unwrap();
    client.enable_trigger("deploys", &new_key()).unwrap();
    let body = r#"{"id":"e-5"}"#;
    let (status, _) = deliver(&server, &id, &generic_headers(&secret, body), body);
    assert_eq!(status, 401);
    let (status, _) = deliver(&server, &id, &generic_headers(&rotated, body), body);
    assert_eq!(status, 202);
    assert_eq!(client.remove_trigger("deploys").unwrap().removed, "deploys");
    assert_eq!(
        client.trigger("deploys").unwrap_err().code(),
        Some("unknown_trigger")
    );
}

#[test]
fn prechecks_run_in_a_fresh_worktree_and_skip_with_their_reason() {
    let f = Fixture::new();
    let mut config = f.config();
    with_clock(&mut config);
    let data = config.data_dir.clone();
    // Not allowed: refused when created.
    let server = Server::start(config.clone());
    let client = server.client();
    let mut s = spec("gated", event(EventSource::Generic), "WRITE g.txt=ok");
    s.precheck = Some(Precheck {
        command: "test -f a.txt".into(),
        timeout_seconds: 30,
    });
    let err = client.create_trigger(&s, &new_key()).unwrap_err();
    assert_eq!(err.code(), Some("precheck_not_allowed"));
    drop(client);
    server.stop();

    config.triggers.allow_prechecks = WorkspaceScripts::All;
    let server = Server::start(config);
    let client = server.client();
    let created = client.create_trigger(&s, &new_key()).unwrap();
    let (id, secret) = (created.trigger.id, created.secret.unwrap());
    let mut s2 = spec("gated-off", event(EventSource::Generic), "WRITE h.txt=no");
    s2.precheck = Some(Precheck {
        command: "echo looking; test -f nothing-to-do.txt".into(),
        timeout_seconds: 30,
    });
    let created2 = client.create_trigger(&s2, &new_key()).unwrap();
    let body = r#"{"id":"p-1"}"#;
    assert_eq!(
        deliver(&server, &id, &generic_headers(&secret, body), body).0,
        202
    );
    let secret2 = created2.secret.unwrap();
    assert_eq!(
        deliver(
            &server,
            &created2.trigger.id,
            &generic_headers(&secret2, body),
            body
        )
        .0,
        202
    );
    let passed = runs_when(&client, "gated", "the passing precheck", |runs| {
        runs.iter().any(|r| r.state == RunState::Fired)
    });
    assert_eq!(passed[0].precheck.as_ref().unwrap().exit_code, Some(0));
    let skipped = runs_when(&client, "gated-off", "the failing precheck", |runs| {
        runs.iter().any(|r| r.state == RunState::SkippedPrecheck)
    });
    assert_eq!(
        skipped[0].reason.as_deref(),
        Some("Precheck exited with code 1.")
    );
    assert_eq!(skipped[0].precheck.as_ref().unwrap().stdout, "looking\n");
    // Its worktree is gone, and git no longer lists it.
    let leftovers = std::fs::read_dir(data.join("triggers/prechecks"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(leftovers, 0);
    let worktrees = common::git(&f.root, &["worktree", "list"]);
    assert!(!worktrees.contains("prechecks"), "{worktrees}");
    // A skip is not a failure: nothing counts toward pausing.
    assert_eq!(client.trigger("gated-off").unwrap().consecutive_failures, 0);
}

#[test]
fn slack_tests_and_tenants() {
    let f = Fixture::new();
    let mut config = f.config();
    with_clock(&mut config);
    let appb = f.extra_repo("appb");
    common::two_tenants(&mut config, appb, None);
    let server = Server::start(config);
    let acme = Client::new(&server.url(), common::ACME_TOKEN).unwrap();
    let globex = Client::new(&server.url(), common::GLOBEX_TOKEN).unwrap();
    let mut s = spec(
        "mentions",
        event(EventSource::Slack),
        "Asked in {{event.channel}}: {{event.text}}",
    );
    s.secret = Some("slack-signing-secret".into());
    s.conditions.text_contains = vec!["fix".into()];
    let trigger = acme.create_trigger(&s, &new_key()).unwrap().trigger;
    // Another tenant sees nothing of it, and cannot make one on acme's
    // repository.
    assert_eq!(
        globex.trigger("mentions").unwrap_err().code(),
        Some("unknown_trigger")
    );
    assert!(globex.triggers().unwrap().is_empty());
    assert_eq!(
        globex.create_trigger(&s, &new_key()).unwrap_err().code(),
        Some("repo_not_allowed")
    );
    assert_eq!(acme.triggers().unwrap().len(), 1);
    // A second of the same name in the tenant is refused.
    assert_eq!(
        acme.create_trigger(&s, &new_key()).unwrap_err().code(),
        Some("trigger_exists")
    );

    // Slack's URL verification, signed, within the replay window.
    let ts = (T0 / 1000).to_string();
    let challenge = r#"{"type":"url_verification","challenge":"c-123","token":"t"}"#;
    let slack = |body: &str, ts: &str| {
        format!(
            "X-Slack-Request-Timestamp: {ts}\r\nX-Slack-Signature: v0={}\r\n",
            sign(
                "slack-signing-secret",
                &[b"v0:", ts.as_bytes(), b":", body.as_bytes()]
            )
        )
    };
    let (status, answer) = deliver(&server, &trigger.id, &slack(challenge, &ts), challenge);
    assert_eq!((status, answer["challenge"].as_str()), (200, Some("c-123")));
    let old = (T0 / 1000 - 3600).to_string();
    let (status, answer) = deliver(&server, &trigger.id, &slack(challenge, &old), challenge);
    assert_eq!(status, 401);
    assert_eq!(answer["error"]["code"], "stale_delivery");

    // A test renders the task and creates nothing.
    let mention = serde_json::json!({
        "type": "event_callback", "event_id": "Ev9",
        "event": {"type": "app_mention", "user": "U1", "text": "<@B> fix the build", "channel": "C42"}
    });
    let tested = acme
        .test_trigger(
            "mentions",
            &TriggerTestRequest {
                event: Some(mention),
                ..TriggerTestRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    assert!(tested.would_fire, "{tested:?}");
    assert_eq!(tested.key, "event:Ev9");
    assert_eq!(
        tested.task.unwrap().prompt,
        "Asked in C42: <@B> fix the build"
    );
    let other = serde_json::json!({
        "type": "event_callback", "event_id": "Ev10",
        "event": {"type": "app_mention", "user": "U1", "text": "hello", "channel": "C42"}
    });
    let tested = acme
        .test_trigger(
            "mentions",
            &TriggerTestRequest {
                event: Some(other),
                ..TriggerTestRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    assert!(!tested.would_fire);
    assert!(tested.reason.unwrap().contains("does not contain fix"));
    assert!(acme.trigger_runs("mentions", 10).unwrap().is_empty());
    assert!(acme.repo("app").branches().unwrap().is_empty());

    // Validation at creation: an unknown placeholder, a bad cron.
    let mut bad = spec("bad", event(EventSource::Slack), "{{event.titel}}");
    bad.secret = Some("x".into());
    let err = acme.create_trigger(&bad, &new_key()).unwrap_err();
    assert_eq!(err.code(), Some("invalid_request"));
    let bad = spec(
        "bad",
        When::Cron {
            expr: "0 9 * *".into(),
            timezone: "UTC".into(),
        },
        "x",
    );
    assert_eq!(
        acme.create_trigger(&bad, &new_key()).unwrap_err().code(),
        Some("invalid_request")
    );
}

#[test]
fn a_run_recorded_before_a_restart_fires_after_it() {
    let f = Fixture::new();
    let mut config = f.config();
    with_clock(&mut config);
    // The first server records deliveries but runs no dispatcher, as if it
    // stopped before firing.
    config.triggers.dispatch = false;
    let server = Server::start(config.clone());
    let client = server.client();
    let created = client
        .create_trigger(
            &spec("later", event(EventSource::Generic), "WRITE later.txt=1"),
            &new_key(),
        )
        .unwrap();
    let body = r#"{"id":"r-1"}"#;
    let headers = generic_headers(created.secret.as_ref().unwrap(), body);
    assert_eq!(deliver(&server, &created.trigger.id, &headers, body).0, 202);
    wait::settle(
        "a delivery that must stay pending would have moved by now",
        Duration::from_millis(200),
    );
    assert_eq!(
        client.trigger_runs("later", 10).unwrap()[0].state,
        RunState::Pending
    );
    drop(client);
    server.stop();

    config.triggers.dispatch = true;
    let server = Server::start(config);
    let client = server.client();
    let runs = runs_when(&client, "later", "the pending run to fire", |runs| {
        runs[0].state == RunState::Fired
    });
    assert_eq!(runs.len(), 1);
    assert_eq!(
        await_operation(&client, runs[0].operation.as_ref().unwrap()).state,
        OperationState::Succeeded
    );
}

/// A delivery to `path` with its own content type.
fn deliver_mail(
    server: &Server,
    path: &str,
    content_type: &str,
    extra: &str,
    body: &str,
) -> (u16, serde_json::Value) {
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{extra}\
         Content-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let (status, _, text) = raw(server.addr, &request);
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

/// A form value, URL-encoded.
fn encoded(value: &str) -> String {
    branchyard_client::http::encode(value).replace("%20", "+")
}

/// Mailgun's form for one message, signed with `key` at `timestamp`
/// (seconds) with `token`.
fn mailgun_form(key: &str, timestamp: u64, token: &str, from: &str, message_id: &str) -> String {
    let headers = serde_json::json!([
        ["From", from],
        ["To", "agent@by.example"],
        ["Subject", "Nightly broke"],
        ["Message-Id", format!("<{message_id}>")]
    ])
    .to_string();
    let signature = sign(key, &[timestamp.to_string().as_bytes(), token.as_bytes()]);
    [
        ("recipient", "agent@by.example".to_owned()),
        ("from", from.to_owned()),
        ("subject", "Nightly broke".to_owned()),
        ("body-plain", "The nightly job failed at step 3.".to_owned()),
        ("message-headers", headers),
        ("timestamp", timestamp.to_string()),
        ("token", token.to_owned()),
        ("signature", signature),
    ]
    .iter()
    .map(|(name, value)| format!("{name}={}", encoded(value)))
    .collect::<Vec<_>>()
    .join("&")
}

/// Email triggers through the server: a Mailgun delivery fires a task
/// once however it is redelivered, a token replayed with another message
/// is refused, unsigned deliveries and senders off the allowlist record
/// nothing; a Postmark delivery is authenticated by its URL's token or a
/// Basic password. On the store `config` names.
fn email_deliveries_fire_once(config: Config) {
    let mut config = config;
    let clock = with_clock(&mut config);
    let server = Server::start(config);
    let client = server.client();
    let form = "application/x-www-form-urlencoded";

    let mut s = spec(
        "mail",
        event(EventSource::Mailgun),
        "WRITE mail.txt={{event.from}}",
    );
    s.secret = Some("mg-key".into());
    s.conditions = Conditions {
        sender: vec!["@partner.example".into()],
        recipient: vec!["agent@by.example".into()],
        ..Conditions::default()
    };
    s.task.name = Some("mail-reply".into());
    let id = client.create_trigger(&s, &new_key()).unwrap().trigger.id;
    let path = format!("/v1/triggers/{id}/fire");
    let now = T0 / 1000;

    let body = mailgun_form(
        "mg-key",
        now,
        "tok-1",
        "Bob <bob@partner.example>",
        "m1@partner.example",
    );
    let (status, ack) = deliver_mail(&server, &path, form, "", &body);
    assert_eq!(status, 202, "{ack}");
    assert_eq!(ack["run"]["key"], "event:m1@partner.example");
    let runs = runs_when(&client, "mail", "the email to fire", |runs| {
        runs.iter().any(|r| r.state == RunState::Fired)
    });
    let fired = runs[0].clone();
    assert_eq!(fired.branches, ["mail-reply"]);
    let email = fired.event.as_ref().unwrap().email.as_ref().unwrap();
    assert_eq!(email.from, "bob@partner.example");
    let op = await_operation(&client, fired.operation.as_ref().unwrap());
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    let diff = client.repo("app").diff("mail-reply").unwrap();
    assert!(diff.contains("+bob@partner.example"), "{diff}");

    // The same delivery again, and Mailgun's retry of the message with a
    // fresh token and timestamp: the run the first recorded.
    clock.store(T0 + 60_000, Ordering::SeqCst);
    let retry = mailgun_form(
        "mg-key",
        now + 60,
        "tok-2",
        "Bob <bob@partner.example>",
        "m1@partner.example",
    );
    for again in [&body, &retry] {
        let (status, ack) = deliver_mail(&server, &path, form, "", again);
        assert_eq!(status, 200, "{ack}");
        assert_eq!(ack["duplicate"], true);
        assert_eq!(ack["run"]["id"], serde_json::json!(fired.id));
    }
    // A captured token with another message: Mailgun signs only the
    // timestamp and token, so the signature holds, but the token is spent.
    let forged = mailgun_form(
        "mg-key",
        now,
        "tok-1",
        "Bob <bob@partner.example>",
        "m2@partner.example",
    );
    let (status, ack) = deliver_mail(&server, &path, form, "", &forged);
    assert_eq!(
        (status, ack["error"]["code"].as_str()),
        (401, Some("stale_delivery")),
        "{ack}"
    );
    // Too old, unsigned, or signed with another key: refused.
    let old = mailgun_form(
        "mg-key",
        now - 3600,
        "tok-3",
        "Bob <bob@partner.example>",
        "m3@partner.example",
    );
    let (status, ack) = deliver_mail(&server, &path, form, "", &old);
    assert_eq!(
        (status, ack["error"]["code"].as_str()),
        (401, Some("stale_delivery"))
    );
    let unsigned = "from=bob%40partner.example&subject=x&body-plain=y";
    let (status, ack) = deliver_mail(&server, &path, form, "", unsigned);
    assert_eq!(
        (status, ack["error"]["code"].as_str()),
        (401, Some("invalid_signature"))
    );
    let other_key = mailgun_form(
        "not-it",
        now + 60,
        "tok-4",
        "Bob <bob@partner.example>",
        "m4@partner.example",
    );
    let (status, _) = deliver_mail(&server, &path, form, "", &other_key);
    assert_eq!(status, 401);
    // A signed message from a sender off the allowlist: answered, recorded
    // nowhere, so the provider does not retry it.
    let stranger = mailgun_form(
        "mg-key",
        now + 60,
        "tok-5",
        "Eve <eve@evil.example>",
        "m5@evil.example",
    );
    let (status, ack) = deliver_mail(&server, &path, form, "", &stranger);
    assert_eq!(status, 200, "{ack}");
    assert!(
        ack["ignored"]
            .as_str()
            .unwrap()
            .contains("eve@evil.example is not on this trigger's allowlist"),
        "{ack}"
    );
    let runs = client.trigger_runs("mail", 50).unwrap();
    assert_eq!(runs.len(), 1, "one run in all of that: {runs:?}");

    // Postmark signs nothing: the secret in the URL, or as a password.
    let mut p = spec(
        "postmark",
        event(EventSource::Postmark),
        "WRITE pm.txt={{event.message_id}}",
    );
    p.conditions.sender = vec!["carol@example.com".into()];
    p.conditions.subject_contains = vec!["[agent]".into()];
    let created = client.create_trigger(&p, &new_key()).unwrap();
    let secret = created.secret.clone().expect("a generated secret");
    let pid = created.trigger.id;
    let message = |subject: &str, message_id: &str| {
        serde_json::json!({
            "From": "carol@example.com",
            "FromFull": {"Email": "carol@example.com", "Name": "Carol"},
            "To": "agent@by.example",
            "Subject": subject,
            "MessageID": "pm-1",
            "TextBody": "please rename the flag",
            "Headers": [{"Name": "Message-ID", "Value": format!("<{message_id}>")}]
        })
        .to_string()
    };
    let body = message("[agent] rename", "p1@example.com");
    let json = "application/json";
    let (status, _) = deliver_mail(
        &server,
        &format!("/v1/triggers/{pid}/fire/wrong"),
        json,
        "",
        &body,
    );
    assert_eq!(status, 401);
    let (status, _) = deliver_mail(
        &server,
        &format!("/v1/triggers/{pid}/fire"),
        json,
        "",
        &body,
    );
    assert_eq!(status, 401, "no password");
    let (status, ack) = deliver_mail(
        &server,
        &format!("/v1/triggers/{pid}/fire/{secret}"),
        json,
        "",
        &body,
    );
    assert_eq!(status, 202, "{ack}");
    let basic = format!(
        "Authorization: Basic {}\r\n",
        base64::engine::general_purpose::STANDARD.encode(format!("branchyard:{secret}"))
    );
    let (status, ack) = deliver_mail(
        &server,
        &format!("/v1/triggers/{pid}/fire"),
        json,
        &basic,
        &body,
    );
    assert_eq!(
        (status, &ack["duplicate"]),
        (200, &serde_json::json!(true)),
        "{ack}"
    );
    // Another subject: recorded as skipped by condition.
    let other = message("hello", "p2@example.com");
    let (status, ack) = deliver_mail(
        &server,
        &format!("/v1/triggers/{pid}/fire"),
        json,
        &basic,
        &other,
    );
    assert_eq!(status, 200);
    assert_eq!(ack["run"]["state"], "skipped_condition");
    // A trigger of another source has no URL with a token.
    let mut g = spec("generic", event(EventSource::Generic), "x");
    g.secret = Some(secret.clone());
    let gid = client.create_trigger(&g, &new_key()).unwrap().trigger.id;
    let (status, _) = deliver_mail(
        &server,
        &format!("/v1/triggers/{gid}/fire/{secret}"),
        json,
        "",
        &body,
    );
    assert_eq!(status, 404);
    let runs = runs_when(&client, "postmark", "the email to fire", |runs| {
        runs.iter().any(|r| r.state == RunState::Fired)
    });
    let fired = runs.iter().find(|r| r.state == RunState::Fired).unwrap();
    let op = await_operation(&client, fired.operation.as_ref().unwrap());
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    assert_eq!(runs.len(), 2, "{runs:?}");
}

#[test]
fn email_deliveries_are_verified_allowlisted_and_fire_once() {
    let f = Fixture::new();
    email_deliveries_fire_once(f.config());
}

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use branchyard_server::triggers::store::{conformance, PostgresTriggers};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn database() -> Option<String> {
        let Some(base) = std::env::var("BY_TEST_POSTGRES_URL")
            .ok()
            .filter(|u| !u.is_empty())
        else {
            eprintln!("skipped: set BY_TEST_POSTGRES_URL to run the PostgreSQL trigger tests");
            return None;
        };
        let schema = format!(
            "triggers_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let mut client = ::postgres::Client::connect(&base, ::postgres::NoTls).unwrap();
        client
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}"
            ))
            .unwrap();
        let separator = if base.contains('?') { '&' } else { '?' };
        Some(format!("{base}{separator}options=-csearch_path%3D{schema}"))
    }

    #[test]
    fn email_deliveries_on_postgres_fire_once() {
        let Some(url) = database() else { return };
        let f = Fixture::new();
        let mut config = f.config();
        config.database = Some(url);
        email_deliveries_fire_once(config);
    }

    #[test]
    fn the_postgres_store_conforms() {
        let Some(url) = database() else { return };
        // Opening twice, concurrently, makes the tables once.
        let opened: Vec<PostgresTriggers> = std::thread::scope(|s| {
            let a = s.spawn(|| PostgresTriggers::open(&url).unwrap());
            let b = s.spawn(|| PostgresTriggers::open(&url).unwrap());
            vec![a.join().unwrap(), b.join().unwrap()]
        });
        conformance::all(&opened[0]);
    }

    #[test]
    fn concurrent_claims_on_postgres_take_a_schedule_time_once() {
        let Some(url) = database() else { return };
        let stores: Vec<PostgresTriggers> = (0..4)
            .map(|_| PostgresTriggers::open(&url).unwrap())
            .collect();
        let t = conformance::trigger("acme", "every", true);
        use branchyard_server::triggers::store::TriggerStore;
        assert!(stores[0].create(&t).unwrap());
        let won: usize = std::thread::scope(|s| {
            let handles: Vec<_> = stores
                .iter()
                .enumerate()
                .map(|(i, store)| {
                    let t = &t;
                    s.spawn(move || {
                        let run = conformance::run(
                            &t.id,
                            &format!("schedule:60000-{i}"),
                            RunState::Pending,
                        );
                        usize::from(
                            store
                                .claim_schedule(&t.id, 60_000, Some(120_000), &[run], "w", 1)
                                .unwrap(),
                        )
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).sum()
        });
        assert_eq!(won, 1);
        assert_eq!(stores[1].runs(&t.id, 10).unwrap().len(), 1);
        // Pending runs too: each is claimed by one of the racing stores.
        let e = conformance::trigger("acme", "events", false);
        assert!(stores[0].create(&e).unwrap());
        for n in 0..8 {
            stores[0]
                .record(&conformance::run(
                    &e.id,
                    &format!("event:{n}"),
                    RunState::Pending,
                ))
                .unwrap();
        }
        let claimed: Vec<String> = std::thread::scope(|s| {
            let handles: Vec<_> = stores
                .iter()
                .map(|store| {
                    s.spawn(move || {
                        let mut mine = Vec::new();
                        while let Some((run, _)) =
                            store.claim_run(&["app".into()], "w", 0, 1_000).unwrap()
                        {
                            mine.push(run.id);
                        }
                        mine
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });
        let mut unique = claimed.clone();
        unique.sort();
        unique.dedup();
        assert_eq!((claimed.len(), unique.len()), (8, 8), "{claimed:?}");
    }

    #[test]
    fn two_servers_on_one_database_fire_a_scheduled_time_once() {
        let Some(url) = database() else { return };
        let f = Fixture::new();
        let mut config = f.config();
        config.database = Some(url);
        let clock = with_clock(&mut config);
        let mut second = config.clone();
        second.data_dir = f.dir.join("data2");
        let a = Server::start(config);
        let b = Server::start(second);
        let client = a.client();
        client
            .create_trigger(
                &spec("both", When::Interval { seconds: 60 }, "WRITE both.txt=1"),
                &new_key(),
            )
            .unwrap();
        clock.store(T0 + 60_000, Ordering::SeqCst);
        let runs = runs_when(&client, "both", "the scheduled run to fire", |runs| {
            runs.iter().any(|r| r.state == RunState::Fired)
        });
        wait::settle(
            "the other server would have fired too, if it was going to",
            Duration::from_millis(500),
        );
        let runs_b = b.client().trigger_runs("both", 10).unwrap();
        assert_eq!(runs_b.len(), 1, "{runs_b:?}");
        assert_eq!(
            await_operation(&client, runs[0].operation.as_ref().unwrap()).state,
            OperationState::Succeeded
        );
    }
}
