//! The server with `database` set: branch state and operations in
//! PostgreSQL, over real HTTP with the fake ACP agent. Runs with the
//! `postgres` feature when `BY_TEST_POSTGRES_URL` names a database the
//! tests may create schemas in; each test gets a schema of its own.

#![cfg(feature = "postgres")]

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use branchyard::BranchStatus;
use branchyard_client::api::{OperationState, SendRequest};
use branchyard_client::new_key;
use common::{eventually, run, task, wait, Fixture, Server};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A fresh schema in the test database, and a URL whose connections use
/// it.
fn database() -> Option<String> {
    let Some(base) = std::env::var("BY_TEST_POSTGRES_URL")
        .ok()
        .filter(|u| !u.is_empty())
    else {
        eprintln!("skipped: set BY_TEST_POSTGRES_URL to run the PostgreSQL server tests");
        return None;
    };
    let schema = format!(
        "server_{}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let mut client = postgres::Client::connect(&base, postgres::NoTls).unwrap();
    client
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}"
        ))
        .unwrap();
    let separator = if base.contains('?') { '&' } else { '?' };
    Some(format!("{base}{separator}options=-csearch_path%3D{schema}"))
}

#[test]
fn a_server_keeps_branches_and_operations_in_postgres_across_a_restart() {
    let Some(url) = database() else { return };
    let f = Fixture::new();
    let mut config = f.config();
    config.database = Some(url.clone());
    let server = Server::start(config.clone());
    let client = server.client();
    let repo = client.repo("app");

    let first = run(&client, &task("WRITE hello.txt=hi", "hello"));
    assert_eq!(first.state, OperationState::Succeeded, "{first:?}");
    let sent = wait(
        &client,
        &repo
            .send(
                "hello",
                &SendRequest {
                    prompt: "WHOAMI".into(),
                    ..SendRequest::default()
                },
                &new_key(),
            )
            .unwrap()
            .id,
    );
    assert_eq!(sent.state, OperationState::Succeeded);

    // A held turn, cancelled through the server.
    let held = repo.submit_task(&task("HANG", "held"), &new_key()).unwrap();
    eventually("the held turn to start", || {
        repo.events("held", 0)
            .map(|e| {
                e.events
                    .iter()
                    .any(|e| matches!(e.activity, branchyard::Activity::Prompt(_)))
            })
            .unwrap_or(false)
    });
    assert_eq!(repo.cancel("held").unwrap(), ["held"]);
    let held = wait(&client, &held.id);
    assert_eq!(
        held.result.unwrap().branches[0].status,
        BranchStatus::Interrupted
    );

    // The event feed is read from the database by cursor.
    let mut stream = repo.stream(Some(0));
    let entry = stream.next().unwrap().unwrap();
    assert_eq!((entry.seq, entry.branch.as_str()), (1, "hello"));
    drop(stream);
    let events = repo.events("hello", 0).unwrap();
    assert!(events.cursor > 3);

    // Nothing went to SQLite.
    assert!(!f.data.join("state.db").exists());
    assert!(!f.root.join(".branchyard/state.db").exists());
    drop(server);

    let server = Server::start(config);
    let client = server.client();
    assert_eq!(client.operation(&first.id).unwrap(), first);
    let branches = client.repo("app").branches().unwrap();
    let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, ["hello", "held"]);
    let merged = wait(
        &client,
        &client
            .repo("app")
            .merge("hello", &Default::default(), &new_key())
            .unwrap()
            .id,
    );
    assert_eq!(merged.state, OperationState::Succeeded, "{merged:?}");
    assert_eq!(common::git(&f.root, &["show", "main:hello.txt"]), "hi\n");
}

#[test]
fn a_server_marks_operations_interrupted_in_postgres() {
    let Some(url) = database() else { return };
    let f = Fixture::new();
    let mut config = f.config();
    config.database = Some(url);
    config.shutdown_grace = Duration::ZERO;
    let server = Server::start(config.clone());
    let mut long = task("HANG", "long");
    long.budget.max_seconds = Some(2.0);
    let op = server
        .client()
        .repo("app")
        .submit_task(&long, &new_key())
        .unwrap();
    eventually("the operation to run", || {
        server.client().operation(&op.id).unwrap().state == OperationState::Running
    });
    assert_eq!(server.stop().interrupted, 1);
    let server = Server::start(config);
    let client = server.client();
    let op = client.operation(&op.id).unwrap();
    assert_eq!(op.state, OperationState::Interrupted);
    assert_eq!(op.error.unwrap().code, "interrupted");
    // The turn's thread outlives the stopped server in this test process;
    // let it finish before cleaning up.
    eventually("the orphaned turn to end", || {
        client
            .repo("app")
            .branch("long")
            .is_ok_and(|b| b.status != BranchStatus::Running)
    });
}
