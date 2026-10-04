//! A served repository's warm pool over real HTTP with the fake ACP agent:
//! the server keeps it full when it carries the pool's labels, a task's
//! branch takes a ready slot, the server refills after the claim, and
//! `/metrics` reports the slots, the hit, the fills and the start latency
//! by pool use.

#![allow(clippy::expect_used, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;

use branchyard::{Activity, PoolSlotState, WorkspacePhase};
use branchyard_client::api::OperationState;
use branchyard_server::config::{MetricsConfig, WorkspaceScripts};
use branchyard_testkit::wait;
use common::{get, raw, run, task, Fixture, Server, TOKEN};

const PROJECT: &str = r#"
[workspace]
setup = "mkdir -p deps && echo built > deps/lib.txt"
prepare = true

[workspace.pool]
size = 1
labels = ["warm"]
"#;

fn fixture() -> Fixture {
    let f = Fixture::new();
    fs::write(f.root.join(".gitignore"), "branchyard.toml\ndeps/\n").unwrap();
    common::git(&f.root, &["add", ".gitignore"]);
    common::git(&f.root, &["commit", "-q", "-m", "ignore"]);
    fs::write(f.root.join("branchyard.toml"), PROJECT).unwrap();
    f
}

fn ready(f: &Fixture) -> Vec<String> {
    branchyard::Yard::open(&f.root)
        .unwrap()
        .pool_slots()
        .unwrap()
        .into_iter()
        .filter(|s| s.state == PoolSlotState::Ready)
        .map(|s| s.id)
        .collect()
}

/// The value of the sample `name{labels}` in `text`.
fn sample(text: &str, name_and_labels: &str) -> Option<f64> {
    text.lines()
        .find_map(|line| line.strip_prefix(&format!("{name_and_labels} ")))
        .map(|v| v.parse().unwrap())
}

fn pool_use(client: &branchyard_client::Client, name: &str) -> branchyard::PoolUse {
    let events = client.repo("app").events(name, 0).unwrap().events;
    *events
        .iter()
        .find_map(|e| match &e.activity {
            Activity::Workspace(r) if r.phase == WorkspacePhase::Setup => r.pool.clone(),
            _ => None,
        })
        .expect("the setup report says how the pool was used")
}

#[test]
fn the_server_keeps_the_pool_full_and_a_task_starts_from_it() {
    let f = fixture();
    let mut config = f.config();
    config.allow_workspace_scripts = WorkspaceScripts::Repos(["app".to_owned()].into());
    config.metrics = Some(MetricsConfig {
        listen: None,
        token_sha256: None,
    });

    // Without the pool's labels the server leaves it alone: a task finds
    // no slot, and none is made after it.
    let server = Server::start(config.clone());
    let client = server.client();
    let done = run(&client, &task("SH cat deps/lib.txt", "cold"));
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let used = pool_use(&client, "cold");
    assert_eq!(used.slot, None);
    assert_eq!(used.reason.as_deref(), Some("no ready slot"));
    let (_, _, text) = raw(server.addr, &get("/metrics", Some(TOKEN)));
    assert_eq!(
        sample(
            &text,
            r#"branchyard_pool_claims_total{repo="app",result="miss"}"#
        ),
        Some(1.0),
        "{text}"
    );
    assert_eq!(
        sample(&text, r#"branchyard_start_seconds_count{pool="miss"}"#),
        Some(1.0),
        "{text}"
    );
    server.stop();
    assert!(ready(&f).is_empty());

    config.labels = vec!["warm".into()];
    let server = Server::start(config);
    let client = server.client();
    wait::until("the server to fill the pool", || ready(&f).len() == 1);
    let first = ready(&f).remove(0);

    let done = run(&client, &task("SH cat deps/lib.txt", "warm"));
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let used = pool_use(&client, "warm");
    assert_eq!(used.slot.as_deref(), Some(first.as_str()), "{used:?}");
    let events = client.repo("app").events("warm", 0).unwrap().events;
    let said: String = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Harness(branchyard::Event::MessageDelta { text, .. }) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(said.contains("built"), "{said}");

    // The claim woke the keeper: a new slot, without a restart.
    wait::until("a refill", || {
        let now = ready(&f);
        now.len() == 1 && now[0] != first
    });
    let (status, _, text) = raw(server.addr, &get("/metrics", Some(TOKEN)));
    assert_eq!(status, 200, "{text}");
    assert_eq!(
        sample(&text, r#"branchyard_pool_slots{repo="app",state="ready"}"#),
        Some(1.0),
        "{text}"
    );
    assert_eq!(
        sample(
            &text,
            r#"branchyard_pool_claims_total{repo="app",result="hit"}"#
        ),
        Some(1.0),
        "{text}"
    );
    assert_eq!(
        sample(&text, r#"branchyard_start_seconds_count{pool="hit"}"#),
        Some(1.0),
        "{text}"
    );
    assert_eq!(
        sample(
            &text,
            r#"branchyard_pool_slots_made_total{repo="app",result="made"}"#
        ),
        Some(2.0),
        "{text}"
    );
    assert_eq!(
        sample(&text, r#"branchyard_pool_fill_seconds_count{repo="app"}"#),
        Some(2.0),
        "{text}"
    );
    server.stop();
}
