//! Worker inventories over real HTTP (docs/harness-lifecycle.md): a worker
//! advertises the harnesses its machine has, `GET /v1/inventory` shows
//! them, the labels they derive (`harness:<id>`) let it claim work that
//! requires them, and a task that runs a harness by name is steered to a
//! worker whose inventory can run it. On SQLite here; on PostgreSQL, with
//! two servers sharing one database, when `BY_TEST_POSTGRES_URL` is set
//! and the `postgres` feature is on. The inventories are canned: no
//! harness is detected or run.

mod common;

use std::sync::Arc;
use std::time::Duration;

use branchyard_client::api::{InventoryReport, OperationState, TaskRequest};
use branchyard_client::{new_key, Client};
use branchyard_server::ops::InventorySource;
use branchyard_server::store::test_inventory;
use common::{eventually, task, wait, Fixture, Server};

/// A source that always finds `ids` installed and logged in, or nothing.
fn source(ids: &[&str]) -> InventorySource {
    let mut inventory = test_inventory(ids.first().copied().unwrap_or("goose"));
    if ids.is_empty() {
        inventory.harnesses.clear();
    }
    for id in ids.iter().skip(1) {
        let mut more = inventory.harnesses[0].clone();
        more.id = (*id).to_owned();
        inventory.checked.push((*id).to_owned());
        inventory.harnesses.push(more);
    }
    InventorySource(Arc::new(move || Some(inventory.clone())))
}

fn advertised(client: &Client, workers: usize) -> InventoryReport {
    eventually("the workers to advertise their inventories", || {
        let report = client.inventory().unwrap();
        report.workers.len() >= workers && report.workers.iter().all(|w| w.inventory.is_some())
    });
    client.inventory().unwrap()
}

fn by_name(harness: &str, name: &str) -> TaskRequest {
    TaskRequest {
        harness: Some(harness.into()),
        ..task("WRITE x.txt=1", name)
    }
}

#[test]
fn a_worker_advertises_its_harnesses_and_work_follows_them_on_sqlite() {
    let f = Fixture::new();
    let mut config = f.config();
    config.inventory_source = Some(source(&[]));
    config.unclaimable_after = Duration::ZERO;
    let server = Server::start(config);
    let client = server.client();
    let report = advertised(&client, 1);
    assert!(report.workers[0].this);
    assert_eq!(report.workers[0].repos, ["app"]);
    assert!(report.workers[0].labels.is_empty());

    // Work that requires Codex waits, and says why.
    let mut needs = task("WRITE needs.txt=1", "needs-codex");
    needs.require_labels = vec!["harness:codex".into()];
    let op = client.repo("app").submit_task(&needs, &new_key()).unwrap();
    assert_eq!(op.requires, ["harness:codex"]);
    eventually("the operation to say why it waits", || {
        client.operation(&op.id).unwrap().waiting.is_some()
    });
    let reason = client.operation(&op.id).unwrap().waiting.unwrap();
    assert!(reason.contains("harness:codex"), "{reason}");
    // A task naming Codex is not held back when no worker advertises it.
    let plain = client
        .repo("app")
        .submit_task(&by_name("codex", "codex-anywhere"), &new_key())
        .unwrap();
    assert!(plain.requires.is_empty(), "{:?}", plain.requires);
    wait(&client, &plain.id);

    // A worker whose machine has Codex claims it with no label configured.
    server.stop();
    let mut config = f.config();
    config.inventory_source = Some(source(&["codex"]));
    let server = Server::start(config);
    let client = server.client();
    let done = wait(&client, &op.id);
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let report = advertised(&client, 1);
    let me = report.workers.iter().find(|w| w.this).unwrap();
    assert_eq!(me.labels, ["harness:codex"]);
    let codex = me.inventory.as_ref().unwrap().get("codex").unwrap();
    assert_eq!(codex.version.as_deref(), Some("1.2.3"));

    // Now a task that runs Codex by name requires it; one for a harness no
    // worker has, or with the server's own command, does not.
    let steered = client
        .repo("app")
        .submit_task(&by_name("codex", "codex-steered"), &new_key())
        .unwrap();
    assert_eq!(steered.requires, ["harness:codex"]);
    wait(&client, &steered.id);
    let goose = client
        .repo("app")
        .submit_task(&by_name("goose", "goose-anywhere"), &new_key())
        .unwrap();
    assert!(goose.requires.is_empty());
    wait(&client, &goose.id);
    let commanded = client
        .repo("app")
        .submit_task(&task("WRITE c.txt=1", "commanded"), &new_key())
        .unwrap();
    assert!(commanded.requires.is_empty());
    wait(&client, &commanded.id);
}

#[test]
fn a_server_without_inventory_advertises_none() {
    let f = Fixture::new();
    let mut config = f.config();
    config.inventory = false;
    config.inventory_source = Some(source(&["codex"]));
    let server = Server::start(config);
    let client = server.client();
    eventually("the worker to beat", || {
        !client.inventory().unwrap().workers.is_empty()
    });
    let report = client.inventory().unwrap();
    assert_eq!(report.workers[0].inventory, None);
    assert!(report.workers[0].labels.is_empty());
}

#[cfg(feature = "postgres")]
mod postgres {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn database() -> Option<String> {
        let Some(base) = std::env::var("BY_TEST_POSTGRES_URL")
            .ok()
            .filter(|u| !u.is_empty())
        else {
            eprintln!("skipped: set BY_TEST_POSTGRES_URL to run the PostgreSQL inventory test");
            return None;
        };
        let schema = format!(
            "inventory_{}_{}",
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
    fn workers_on_one_database_advertise_inventories_and_work_follows_them() {
        let Some(url) = database() else { return };
        let f = Fixture::new();
        let mut config = f.config();
        config.database = Some(url.clone());
        config.inventory_source = Some(source(&[]));
        config.unclaimable_after = Duration::ZERO;
        let server = Server::start(config.clone());
        let client = server.client();
        advertised(&client, 1);
        let mut needs = task("WRITE needs.txt=1", "needs-codex");
        needs.require_labels = vec!["harness:codex".into()];
        let op = client.repo("app").submit_task(&needs, &new_key()).unwrap();
        eventually("the operation to say why it waits", || {
            client.operation(&op.id).unwrap().waiting.is_some()
        });

        // A worker with Codex joins the database and takes it.
        let mut worker = config.clone();
        worker.data_dir = f.dir.join("worker");
        worker.worker_only = true;
        worker.inventory_source = Some(source(&["codex"]));
        let worker = Server::start(worker);
        let done = wait(&client, &op.id);
        assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
        let report = advertised(&client, 2);
        assert_eq!(report.workers.len(), 2, "{report:?}");
        assert!(report.workers[0].this);
        assert!(report.workers[0].labels.is_empty());
        assert_eq!(report.workers[1].labels, ["harness:codex"]);
        // A task that runs Codex by name is steered to it.
        let steered = client
            .repo("app")
            .submit_task(&by_name("codex", "codex-steered"), &new_key())
            .unwrap();
        assert_eq!(steered.requires, ["harness:codex"]);
        wait(&client, &steered.id);
        drop(worker);
    }
}
