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

// Durable dispatch: admission is an enqueue in the database, and every
// server or worker on it runs what is queued, once.

use branchyard_client::api::{Operation, OperationKind, TaskRequest};
use branchyard_client::Client;
use branchyard_server::config::{Principal, TenantPolicy};
use branchyard_server::ops::{NewOperation, Options, Registry, WORKER_LOST};
use branchyard_server::store::{
    Admission, AdmissionQuota, Idempotency, OperationStore, PostgresStore, StoredOperation, Worker,
};
use branchyard_server::work::Work;

/// Prompts the branch's harness was sent: one per turn that ran.
fn prompts(client: &Client, branch: &str) -> usize {
    client
        .repo("app")
        .events(branch, 0)
        .unwrap()
        .events
        .iter()
        .filter(|e| matches!(e.activity, branchyard::Activity::Prompt(_)))
        .count()
}

/// Whether the branch exists and its harness has been sent a prompt.
fn prompts_seen(client: &Client, branch: &str) -> bool {
    client.repo("app").events(branch, 0).is_ok_and(|e| {
        e.events
            .iter()
            .any(|e| matches!(e.activity, branchyard::Activity::Prompt(_)))
    })
}

/// Admit a task straight into the database, as a server that crashed
/// right after committing the admission would leave it: queued, never
/// claimed.
fn admit_only(url: &str, request: TaskRequest, key: &str) -> Operation {
    let store = PostgresStore::open(url).unwrap();
    let registry = Registry::open(
        Box::new(store),
        Options {
            exclusive: false,
            ..Options::new(vec!["app".into()])
        },
    )
    .unwrap();
    let name = request.name.clone().unwrap();
    let (op, replayed) = registry
        .submit(
            NewOperation {
                repo: "app".into(),
                kind: OperationKind::Task,
                branches: vec![name.clone()],
                cursor: 0,
                locks: vec![name],
                idempotency: Some(branchyard_server::store::Idempotency {
                    caller: "tester".into(),
                    key: key.into(),
                    fingerprint: "admitted-directly".into(),
                }),
                principal: Principal::default_for("tester"),
                creates: Vec::new(),
                quota: AdmissionQuota::default(),
                requires: Vec::new(),
                priority: 0,
                trace: None,
            },
            Work::Task { request }.to_value().unwrap(),
        )
        .unwrap();
    assert!(!replayed);
    op
}

/// A second server's configuration on the same repository and database,
/// with a data directory of its own.
fn second(
    f: &Fixture,
    config: &branchyard_server::Config,
    name: &str,
) -> branchyard_server::Config {
    let mut other = config.clone();
    other.data_dir = f.dir.join(name);
    other
}

#[test]
fn a_crash_between_admission_and_execution_is_run_once_by_a_worker() {
    let Some(url) = database() else { return };
    let f = Fixture::new();
    let op = admit_only(&url, task("WRITE crash.txt=once", "crash"), "lost-response");
    // Nothing runs it until a worker process starts on the database.
    let mut worker = second(&f, &f.config(), "worker");
    worker.database = Some(url.clone());
    worker.worker_only = true;
    let worker = Server::start(worker);
    assert!(worker.running.as_ref().unwrap().is_worker());

    let mut config = f.config();
    config.database = Some(url);
    let server = Server::start(config);
    let client = server.client();
    let done = wait(&client, &op.id);
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    assert_eq!(prompts(&client, "crash"), 1, "run exactly once");
    // The client that lost the response finds the operation by its key.
    assert_eq!(client.operation_by_key("lost-response").unwrap().id, op.id);
    let missing = client.operation_by_key("never-sent").unwrap_err();
    assert_eq!(missing.code(), Some("unknown_operation"));
    drop(worker);
}

#[test]
fn two_servers_on_one_database_run_each_operation_once() {
    let Some(url) = database() else { return };
    let f = Fixture::new();
    let mut config = f.config();
    config.database = Some(url);
    let a = Server::start(config.clone());
    let b = Server::start(second(&f, &config, "data-b"));
    let (ca, cb) = (a.client(), b.client());

    let mut ops = Vec::new();
    for n in 0..6 {
        let client = if n % 2 == 0 { &ca } else { &cb };
        let request = task(&format!("WRITE f{n}.txt={n}"), &format!("b{n}"));
        ops.push(
            client
                .repo("app")
                .submit_task(&request, &new_key())
                .unwrap(),
        );
    }
    for (n, op) in ops.iter().enumerate() {
        // Either server answers for any operation.
        let other = if n % 2 == 0 { &cb } else { &ca };
        let done = wait(other, &op.id);
        assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
        assert_eq!(prompts(&ca, &format!("b{n}")), 1, "b{n} ran once");
    }

    // Idempotent replay across servers: the retry of a request sent to one
    // server, sent to the other, is the same operation.
    let key = new_key();
    let first = ca
        .repo("app")
        .submit_task(&task("WRITE k.txt=k", "keyed"), &key)
        .unwrap();
    let again = cb
        .repo("app")
        .submit_task(&task("WRITE k.txt=k", "keyed"), &key)
        .unwrap();
    assert_eq!(first.id, again.id);
    let (status, head, _) = common::raw(
        b.addr,
        &common::post(
            "/v1/repos/app/tasks",
            Some(common::TOKEN),
            &format!("Idempotency-Key: {key}\r\n"),
            &serde_json::to_string(&task("WRITE k.txt=k", "keyed")).unwrap(),
        ),
    );
    assert!(status == 200 || status == 202, "{status} {head}");
    assert!(head.contains("idempotent-replayed: true"), "{head}");
    assert_eq!(cb.operation_by_key(&key).unwrap().id, first.id);
    let done = wait(&cb, &first.id);
    assert_eq!(done.state, OperationState::Succeeded);
    assert_eq!(prompts(&ca, "keyed"), 1);

    // Branch locks hold across servers: while one server's operation runs
    // a branch, the other refuses to change it.
    let held = ca
        .repo("app")
        .submit_task(&task("HANG", "held"), &new_key())
        .unwrap();
    eventually("the held turn to start", || prompts_seen(&cb, "held"));
    let send = SendRequest {
        prompt: "WHOAMI".into(),
        ..SendRequest::default()
    };
    let busy = cb.repo("app").send("held", &send, &new_key()).unwrap_err();
    assert_eq!(busy.code(), Some("branch_busy"), "{busy:?}");
    let busy = cb.repo("app").remove("held").unwrap_err();
    assert_eq!(busy.code(), Some("branch_busy"), "{busy:?}");
    assert_eq!(cb.repo("app").cancel("held").unwrap(), ["held"]);
    wait(&cb, &held.id);
    // Released with the operation's outcome: the other server may now.
    let sent = wait(
        &cb,
        &cb.repo("app").send("held", &send, &new_key()).unwrap().id,
    );
    assert_eq!(sent.state, OperationState::Succeeded, "{sent:?}");
}

#[test]
fn an_expired_claim_is_taken_over_and_a_started_operation_is_not_run_again() {
    let Some(url) = database() else { return };
    let f = Fixture::new();
    let unstarted = admit_only(&url, task("WRITE u.txt=u", "unstarted"), "k1");
    let started = admit_only(&url, task("WRITE s.txt=s", "started"), "k2");
    // A worker on another host claims both, records one as running, and
    // dies without renewing.
    let store = PostgresStore::open(&url).unwrap();
    let ghost = Worker {
        id: "ghost".into(),
        host: "another-host/boot".into(),
        ..Worker::current()
    };
    let lease = Duration::from_millis(500);
    let repos = ["app".to_owned()];
    let first = store.claim(&ghost, &repos, &[], lease).unwrap().unwrap();
    let second = store.claim(&ghost, &repos, &[], lease).unwrap().unwrap();
    assert_eq!(first.operation.operation.id, unstarted.id);
    assert_eq!(second.operation.operation.id, started.id);
    let mut running = second.operation.clone();
    running.operation.state = OperationState::Running;
    assert!(store.start(&ghost, second.fence, &running, lease).unwrap());

    let mut config = f.config();
    config.database = Some(url);
    config.operation_lease = Duration::from_secs(2);
    let server = Server::start(config);
    let client = server.client();
    let done = wait(&client, &unstarted.id);
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    assert_eq!(prompts(&client, "unstarted"), 1);
    let taken = wait(&client, &started.id);
    assert_eq!(taken.state, OperationState::Interrupted);
    assert_eq!(taken.error.unwrap().message, WORKER_LOST);
    assert!(client.repo("app").branch("started").is_err(), "never run");
    // The dead worker's late outcome is fenced out.
    let mut late = second.operation.clone();
    late.operation.state = OperationState::Succeeded;
    assert!(!store.finish(&ghost, second.fence, &late).unwrap());
    assert_eq!(
        client.operation(&started.id).unwrap().state,
        OperationState::Interrupted
    );
}

#[test]
fn a_failed_queue_write_rolls_the_admission_back() {
    let Some(url) = database() else { return };
    let f = Fixture::new();
    let mut config = f.config();
    config.database = Some(url.clone());
    let server = Server::start(config);
    let client = server.client();
    let mut db = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    db.batch_execute(
        "CREATE FUNCTION fail_enqueue() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RAISE EXCEPTION 'injected queue failure'; END $$; \
         CREATE TRIGGER fail_enqueue BEFORE INSERT ON by_operation_queue \
         FOR EACH ROW EXECUTE FUNCTION fail_enqueue()",
    )
    .unwrap();
    let key = new_key();
    let request = task("WRITE r.txt=r", "rolled");
    let error = client.repo("app").submit_task(&request, &key).unwrap_err();
    assert_eq!(error.code(), Some("internal"), "{error:?}");
    let count = |table: &str, db: &mut postgres::Client| -> i64 {
        db.query_one(&format!("SELECT COUNT(*) FROM {table}"), &[])
            .unwrap()
            .get(0)
    };
    for table in ["by_operations", "by_operation_queue", "by_branch_locks"] {
        assert_eq!(count(table, &mut db), 0, "{table} kept a row");
    }
    assert_eq!(
        client.operation_by_key(&key).unwrap_err().code(),
        Some("unknown_operation")
    );
    // Once the queue accepts writes, the same retry is admitted and runs.
    db.batch_execute("DROP TRIGGER fail_enqueue ON by_operation_queue")
        .unwrap();
    let op = client.repo("app").submit_task(&request, &key).unwrap();
    assert_eq!(wait(&client, &op.id).state, OperationState::Succeeded);
}

// Tenants on the durable queue: quotas counted in the admission's
// transaction, and operations run as the principal that admitted them.

/// A configuration on `url` whose default tenant may have `tenant_max`
/// operations queued or running at once, running `process_max` at once
/// itself, and shutting down quickly.
fn quota_config(
    f: &Fixture,
    url: &str,
    tenant_max: usize,
    process_max: usize,
) -> branchyard_server::Config {
    let mut config = f.config();
    config.database = Some(url.to_owned());
    config.max_running = process_max;
    config.shutdown_grace = Duration::from_millis(300);
    config.tenants.insert(
        "default".into(),
        TenantPolicy {
            max_running: Some(tenant_max),
            ..TenantPolicy::default()
        },
    );
    config
}

#[test]
fn max_running_holds_across_two_servers_on_one_database() {
    let Some(url) = database() else { return };
    let f = Fixture::new();
    let config = quota_config(&f, &url, 2, 8);
    let a = Server::start(config.clone());
    let b = Server::start(second(&f, &config, "data-b"));
    let urls = [a.url(), b.url()];
    // Eight admissions race, half to each server: exactly two get in.
    let outcomes: Vec<(String, Result<Operation, branchyard_client::Error>)> =
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|n| {
                    let url = urls[n % 2].clone();
                    scope.spawn(move || {
                        let client = Client::new(&url, common::TOKEN).unwrap();
                        let name = format!("race{n}");
                        let result = client
                            .repo("app")
                            .submit_task(&task("HANG", &name), &new_key());
                        (name, result)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
    let mut admitted = Vec::new();
    for (name, outcome) in outcomes {
        match outcome {
            Ok(op) => admitted.push((name, op)),
            Err(e) => assert_eq!(e.code(), Some("quota_exceeded"), "{e:?}"),
        }
    }
    assert_eq!(admitted.len(), 2, "{admitted:?}");
    let (ca, cb) = (a.client(), b.client());
    for (name, _) in &admitted {
        eventually("the admitted turn to start", || {
            common::started(&ca, "app", name)
        });
    }
    let denied = cb
        .repo("app")
        .submit_task(&task("WRITE a.txt=x", "late"), &new_key())
        .unwrap_err();
    assert_eq!(denied.code(), Some("quota_exceeded"));
    assert!(denied.to_string().contains("(2 of 2)"), "{denied}");
    // Released by either server's outcome.
    for (name, op) in &admitted {
        assert_eq!(cb.repo("app").cancel(name).unwrap(), [name.as_str()]);
        assert_eq!(wait(&ca, &op.id).state, OperationState::Succeeded);
    }
    let after = run(&cb, &task("WRITE a.txt=x", "late"));
    assert_eq!(after.state, OperationState::Succeeded, "{after:?}");
}

#[test]
fn max_running_holds_across_a_restart_with_queued_operations() {
    let Some(url) = database() else { return };
    let f = Fixture::new();
    // One turn at a time per process, two per tenant.
    let config = quota_config(&f, &url, 2, 1);
    let a = Server::start(config.clone());
    let client = a.client();
    let first = client
        .repo("app")
        .submit_task(&task("HANG", "h1"), &new_key())
        .unwrap();
    eventually("h1 to start", || common::started(&client, "app", "h1"));
    let queued = client
        .repo("app")
        .submit_task(&task("HANG", "h2"), &new_key())
        .unwrap();
    let denied = client
        .repo("app")
        .submit_task(&task("WRITE a.txt=x", "h3"), &new_key())
        .unwrap_err();
    assert_eq!(denied.code(), Some("quota_exceeded"));
    a.stop();

    // Another server on the database: the queued operation survived and
    // still counts; the interrupted one does not.
    let b = Server::start(second(&f, &config, "data-b"));
    let client = b.client();
    assert_eq!(
        client.operation(&first.id).unwrap().state,
        OperationState::Interrupted
    );
    eventually("h2 to start", || common::started(&client, "app", "h2"));
    let third = client
        .repo("app")
        .submit_task(&task("WRITE a.txt=x", "h3"), &new_key())
        .unwrap();
    let denied = client
        .repo("app")
        .submit_task(&task("WRITE a.txt=y", "h4"), &new_key())
        .unwrap_err();
    assert_eq!(denied.code(), Some("quota_exceeded"));
    assert!(denied.to_string().contains("(2 of 2)"), "{denied}");
    assert_eq!(client.repo("app").cancel("h2").unwrap(), ["h2"]);
    assert_eq!(wait(&client, &queued.id).state, OperationState::Succeeded);
    assert_eq!(wait(&client, &third.id).state, OperationState::Succeeded);
}

fn stored(id: &str, tenant: &str, lock: &str, creates: &[&str]) -> StoredOperation {
    let mut principal = Principal::default_for("ci");
    principal.tenant = tenant.into();
    StoredOperation {
        operation: Operation {
            id: id.into(),
            repo: "app".into(),
            kind: OperationKind::Task,
            state: OperationState::Queued,
            branches: vec![lock.into()],
            cursor: 0,
            end_cursor: None,
            created_at_ms: 1,
            finished_at_ms: None,
            result: None,
            error: None,
            requires: Vec::new(),
            waiting: None,
            priority: 0,
        },
        idempotency: Some(Idempotency {
            caller: format!("{tenant}/ci"),
            key: format!("key-{id}"),
            fingerprint: "f".into(),
        }),
        locks: vec![lock.into()],
        tenant: tenant.into(),
        principal: Some(principal),
        creates: creates.iter().map(|s| s.to_string()).collect(),
        trace: None,
    }
}

#[test]
fn a_quota_refusal_writes_nothing() {
    let Some(url) = database() else { return };
    let store = PostgresStore::open(&url).unwrap();
    let work = serde_json::json!({});
    let running = AdmissionQuota {
        max_running: Some(1),
        ..AdmissionQuota::default()
    };
    assert_eq!(
        store
            .admit(&stored("a", "acme", "x", &[]), &work, &running)
            .unwrap(),
        Admission::Admitted
    );
    assert_eq!(
        store
            .admit(&stored("b", "acme", "y", &[]), &work, &running)
            .unwrap(),
        Admission::Quota {
            limit: "max_running",
            max: 1,
            reserved: 1
        }
    );
    let branches = AdmissionQuota {
        max_branches: Some(2),
        existing_branches: Some(Box::new(|| {
            Ok([("app".to_owned(), "old".to_owned())].into_iter().collect())
        })),
        ..AdmissionQuota::default()
    };
    assert_eq!(
        store
            .admit(
                &stored("c", "globex", "z", &["one", "two"]),
                &work,
                &branches
            )
            .unwrap(),
        Admission::Quota {
            limit: "max_branches",
            max: 2,
            reserved: 1
        }
    );
    let mut db = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    for (table, column) in [
        ("by_operations", "id"),
        ("by_operation_queue", "id"),
        ("by_branch_locks", "token"),
    ] {
        let ids: Vec<String> = db
            .query(&format!("SELECT {column} FROM {table} ORDER BY 1"), &[])
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect();
        assert_eq!(ids, ["a"], "{table}");
    }
    assert!(store.by_key("acme/ci", "key-b").unwrap().is_none());
    assert_eq!(store.unfinished("acme").unwrap().len(), 1);
    assert!(store.unfinished("globex").unwrap().is_empty());
    // The refused admission's branch lock was never taken.
    assert_eq!(
        store
            .hold("app", "y", "a removal", "t", Duration::from_secs(5))
            .unwrap(),
        None
    );
}

#[test]
fn a_worker_runs_another_tenants_operation_as_its_principal_without_leaking_it() {
    let Some(url) = database() else { return };
    let f = Fixture::new();
    let appb = f.extra_repo("appb");
    // Acme's operation, admitted by a server that stopped right after.
    let registry = Registry::open(
        Box::new(PostgresStore::open(&url).unwrap()),
        Options {
            exclusive: false,
            ..Options::new(vec!["app".into()])
        },
    )
    .unwrap();
    let mut acme = Principal::default_for("acme");
    acme.tenant = "acme".into();
    let (op, _) = registry
        .submit(
            NewOperation {
                repo: "app".into(),
                kind: OperationKind::Task,
                branches: vec!["acme-work".into()],
                cursor: 0,
                locks: vec!["acme-work".into()],
                idempotency: Some(Idempotency {
                    caller: "acme/acme".into(),
                    key: "acme-key".into(),
                    fingerprint: "admitted-directly".into(),
                }),
                principal: acme,
                creates: vec!["acme-work".into()],
                quota: AdmissionQuota::default(),
                requires: Vec::new(),
                priority: 0,
                trace: None,
            },
            Work::Task {
                request: task("WRITE w.txt=acme", "acme-work"),
            }
            .to_value()
            .unwrap(),
        )
        .unwrap();
    drop(registry);

    // A worker with no credentials at all runs it.
    let mut worker = second(&f, &f.config(), "worker");
    worker.tokens = Vec::new();
    worker.database = Some(url.clone());
    worker.worker_only = true;
    let worker = Server::start(worker);
    let store = PostgresStore::open(&url).unwrap();
    eventually("the worker to run acme's operation", || {
        store
            .get(&op.id)
            .unwrap()
            .is_some_and(|s| s.operation.state.is_terminal())
    });
    let record = store.get(&op.id).unwrap().unwrap();
    assert_eq!(
        record.operation.state,
        OperationState::Succeeded,
        "{record:?}"
    );
    assert_eq!(record.tenant, "acme");
    assert_eq!(record.principal.as_ref().unwrap().tenant, "acme");
    drop(worker);

    // Only acme sees it, by ID or by key; to globex it does not exist.
    let mut config = f.config();
    config.database = Some(url);
    common::two_tenants(&mut config, appb, None);
    let server = Server::start(config);
    let acme = Client::new(&server.url(), common::ACME_TOKEN).unwrap();
    let globex = Client::new(&server.url(), common::GLOBEX_TOKEN).unwrap();
    assert_eq!(
        acme.operation(&op.id).unwrap().state,
        OperationState::Succeeded
    );
    assert_eq!(acme.operation_by_key("acme-key").unwrap().id, op.id);
    assert_eq!(
        globex.operation(&op.id).unwrap_err().code(),
        Some("unknown_operation")
    );
    assert_eq!(
        globex.operation_by_key("acme-key").unwrap_err().code(),
        Some("unknown_operation")
    );
    assert!(globex.repo("appb").branches().unwrap().is_empty());
    assert_eq!(
        globex.repo("app").branches().unwrap_err().code(),
        Some("repo_not_allowed")
    );
}

// Task graphs under durable dispatch: a spawn that waits is queued work a
// worker runs, and every server and worker on a database resumes graphs
// on its recovery tick without starting a dependent twice.

/// A configuration that allows delegation and recovers every 100 ms.
fn delegating(f: &Fixture, url: &str) -> branchyard_server::Config {
    let mut config = f.config();
    config.database = Some(url.to_owned());
    config.allow_delegation = true;
    config.by_path = Some("/bin/true".into());
    config.recover_interval = Duration::from_millis(100);
    config
}

/// Admit `work` straight into the database, as a server's admission does,
/// for whatever worker runs it.
fn admit(url: &str, work: Work, branches: &[&str], locks: &[&str]) -> Operation {
    let registry = Registry::open(
        Box::new(PostgresStore::open(url).unwrap()),
        Options {
            exclusive: false,
            ..Options::new(vec!["app".into()])
        },
    )
    .unwrap();
    let owned = |names: &[&str]| names.iter().map(|s| (*s).to_owned()).collect();
    let (op, _) = registry
        .submit(
            NewOperation {
                repo: "app".into(),
                kind: work.kind(),
                branches: owned(branches),
                cursor: 0,
                locks: owned(locks),
                idempotency: None,
                principal: Principal::default_for("tester"),
                creates: owned(branches),
                quota: AdmissionQuota::default(),
                requires: Vec::new(),
                priority: 0,
                trace: None,
            },
            work.to_value().unwrap(),
        )
        .unwrap();
    op
}

/// Wait for a queued operation to finish, read from the database: no
/// server answers here, only the worker runs it.
fn finished(url: &str, id: &str) -> Operation {
    let registry = Registry::open(
        Box::new(PostgresStore::open(url).unwrap()),
        Options {
            exclusive: false,
            ..Options::new(vec!["app".into()])
        },
    )
    .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let op = registry.get(id).unwrap().unwrap();
        if op.state.is_terminal() {
            return op;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "operation {id} did not finish"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_spawn_that_waits_is_queued_run_by_a_worker_and_started_later() {
    use branchyard::{After, Envelope, Yard};
    use branchyard_client::api::{PolicySpec, SpawnRequest};
    let Some(url) = database() else { return };
    let f = Fixture::new();
    // Only a worker runs what is queued: no server is started until the
    // end, and this process never runs an operation.
    let mut worker = second(&f, &delegating(&f, &url), "worker");
    worker.worker_only = true;
    let worker = Server::start(worker);

    let root = admit(
        &url,
        Work::Task {
            request: TaskRequest {
                delegation: Some(Envelope::default()),
                ..task("say hi", "root")
            },
        },
        &["root"],
        &["root"],
    );
    assert_eq!(finished(&url, &root.id).state, OperationState::Succeeded);
    let spawn = |name: &str, prompt: &str, depends_on: &[&str]| Work::Spawn {
        parent: "root".into(),
        name: Some(name.into()),
        request: SpawnRequest {
            prompt: prompt.into(),
            name: Some(name.into()),
            policy: PolicySpec::allow_all(),
            depends_on: depends_on.iter().map(|s| (*s).to_owned()).collect(),
            after: After::Integrated,
            ..SpawnRequest::default()
        },
    };
    let lib = admit(
        &url,
        spawn("lib", "WRITE lib.txt=1", &[]),
        &["lib"],
        &["lib", "root"],
    );
    let lib = finished(&url, &lib.id);
    assert_eq!(lib.state, OperationState::Succeeded, "{lib:?}");
    // The description a worker read carried the dependency: the child is
    // created waiting, and the spawn finishes without starting it.
    let app = admit(
        &url,
        spawn("app", "WRITE app.txt=1", &["lib"]),
        &["app"],
        &["app", "root"],
    );
    let app = finished(&url, &app.id);
    assert_eq!(app.state, OperationState::Succeeded, "{app:?}");
    let result = app.result.unwrap();
    assert_eq!(result.branches[0].status, BranchStatus::Waiting);
    let inspection = result.inspection.unwrap();
    assert_eq!(inspection.status, BranchStatus::Waiting);
    assert_eq!(inspection.depends_on.len(), 1);
    assert_eq!(inspection.depends_on[0].prerequisite, "lib");
    assert_eq!(inspection.depends_on[0].after, After::Integrated);
    let yard = Yard::open_postgres(&f.root, &url, "app").unwrap();
    let status = |name: &str| yard.branch(name).unwrap().info().status.clone();
    assert_eq!(status("app"), BranchStatus::Waiting);
    assert_eq!(yard.branch("app").unwrap().info().turns, 0);
    // Several recovery ticks pass: it keeps waiting for the integration.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(status("app"), BranchStatus::Waiting);

    // Integrating lib, also queued work the worker runs, starts app there,
    // from the parent's branch with lib merged in.
    let integrated = admit(
        &url,
        Work::Integrate {
            branch: "lib".into(),
            parent: "root".into(),
        },
        &["lib"],
        &["lib", "root"],
    );
    let integrated = finished(&url, &integrated.id);
    assert_eq!(
        integrated.state,
        OperationState::Succeeded,
        "{integrated:?}"
    );
    eventually("app to start and finish", || {
        status("app") == BranchStatus::Ready
    });
    let app = yard.branch("app").unwrap().info().clone();
    assert_eq!(app.turns, 1);
    assert!(app.worktree.join("lib.txt").is_file(), "built on lib");

    // A server on the database reads the same.
    let server = Server::start(delegating(&f, &url));
    let client = server.client();
    assert_eq!(prompts(&client, "app"), 1);
    assert_eq!(
        client.repo("app").graph("root").unwrap(),
        yard.graph("root").unwrap()
    );
    drop(worker);
}

#[test]
fn servers_and_a_worker_resuming_graphs_on_one_database_start_a_dependent_once() {
    use branchyard::{After, Envelope, GraphEdit, SpawnSpec};
    use branchyard_client::api::{GraphRequest, PolicySpec};
    let Some(url) = database() else { return };
    let f = Fixture::new();
    let config = delegating(&f, &url);
    let a = Server::start(config.clone());
    let b = Server::start(second(&f, &config, "data-b"));
    let mut worker = second(&f, &config, "worker");
    worker.worker_only = true;
    let worker = Server::start(worker);
    let client = a.client();
    let repo = client.repo("app");

    let root = run(
        &client,
        &TaskRequest {
            delegation: Some(Envelope::default()),
            ..task("say hi", "root")
        },
    );
    assert_eq!(root.state, OperationState::Succeeded, "{root:?}");
    let spawn = |name: &str, prompt: &str, depends_on: &[&str]| {
        GraphEdit::Spawn(SpawnSpec {
            prompt: prompt.into(),
            name: Some(name.into()),
            depends_on: depends_on.iter().map(|s| (*s).to_owned()).collect(),
            after: After::Integrated,
            ..SpawnSpec::default()
        })
    };
    let applied = repo
        .apply_graph(
            "root",
            &GraphRequest {
                expected_revision: 0,
                edits: vec![
                    spawn("first", "WRITE f.txt=1", &[]),
                    spawn("second", "say done", &["first"]),
                ],
                policy: PolicySpec::allow_all(),
                unapproved_tools: false,
            },
        )
        .unwrap();
    assert_eq!(applied.spawned[1].status, BranchStatus::Waiting);
    eventually("first to finish", || {
        repo.branch("first").unwrap().status == BranchStatus::Ready
    });
    // Recovery ticks on all three pass over a dependent still waiting.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(repo.branch("second").unwrap().status, BranchStatus::Waiting);
    // The state an engine that stopped between settling the prerequisite
    // and starting the dependent leaves: satisfied, never claimed. Every
    // process's next tick finds it.
    let mut db = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let changed = db
        .execute(
            "UPDATE by_graph_edges SET after = 'settled' WHERE dependent = 'second'",
            &[],
        )
        .unwrap();
    assert_eq!(changed, 1);
    eventually("second to start and settle", || {
        matches!(
            repo.branch("second").unwrap().status,
            BranchStatus::Ready | BranchStatus::NoChanges
        )
    });
    // Many more ticks on every process: still one turn.
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(prompts(&client, "second"), 1, "started exactly once");
    assert_eq!(repo.branch("second").unwrap().turns, 1);
    assert_eq!(prompts(&b.client(), "second"), 1);
    drop(worker);
}

/// The worker-label conformance on PostgreSQL (two workers racing for
/// labeled work through `FOR UPDATE SKIP LOCKED`), on a queue created
/// before the `requires` column existed.
#[test]
fn worker_labels_conform_on_postgres() {
    let Some(url) = database() else { return };
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client
        .batch_execute(
            "CREATE TABLE by_operations (id TEXT PRIMARY KEY, \
                 seq BIGINT GENERATED ALWAYS AS IDENTITY, body TEXT NOT NULL); \
             CREATE TABLE by_operation_queue (id TEXT PRIMARY KEY REFERENCES by_operations (id), \
                 seq BIGINT GENERATED ALWAYS AS IDENTITY, repo TEXT NOT NULL, work TEXT NOT NULL, \
                 attempt BIGINT NOT NULL DEFAULT 0, worker TEXT, host TEXT, pid BIGINT, \
                 start TEXT, lease_until TIMESTAMPTZ)",
        )
        .unwrap();
    drop(client);
    let store = PostgresStore::open(&url).unwrap();
    branchyard_server::store::check_labels(&store, "pg");
}

/// A queue as it was before the `requires` column and the workers table,
/// with one operation queued.
fn old_queue(url: &str) {
    let mut client = postgres::Client::connect(url, postgres::NoTls).unwrap();
    client
        .batch_execute(
            "CREATE TABLE by_operations (id TEXT PRIMARY KEY, \
                 seq BIGINT GENERATED ALWAYS AS IDENTITY, body TEXT NOT NULL); \
             CREATE TABLE by_operation_queue (id TEXT PRIMARY KEY REFERENCES by_operations (id), \
                 seq BIGINT GENERATED ALWAYS AS IDENTITY, repo TEXT NOT NULL, work TEXT NOT NULL, \
                 attempt BIGINT NOT NULL DEFAULT 0, worker TEXT, host TEXT, pid BIGINT, \
                 start TEXT, lease_until TIMESTAMPTZ); \
             INSERT INTO by_operations (id, body) VALUES ('op-old', '{}'); \
             INSERT INTO by_operation_queue (id, repo, work) VALUES ('op-old', 'app', '{}')",
        )
        .unwrap();
}

/// Open the registry from several threads at once.
fn open_together(url: &str, n: usize) {
    let barrier = std::sync::Barrier::new(n);
    std::thread::scope(|scope| {
        let opens: Vec<_> = (0..n)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    PostgresStore::open(url).map(drop)
                })
            })
            .collect();
        for open in opens {
            open.join().unwrap().unwrap();
        }
    });
}

/// The old queue's row, migrated: it requires nothing, has priority 0,
/// belongs to the default tenant and was admitted at the epoch; and the
/// tenants' usage table exists.
fn requires_of_old(url: &str) -> Vec<String> {
    let mut client = postgres::Client::connect(url, postgres::NoTls).unwrap();
    let row = client
        .query_one(
            "SELECT requires, priority, tenant, enqueued_ms FROM by_operation_queue \
             WHERE id = 'op-old'",
            &[],
        )
        .unwrap();
    assert_eq!(row.get::<_, i32>(1), 0);
    assert_eq!(row.get::<_, String>(2), "default");
    assert_eq!(row.get::<_, i64>(3), 0);
    let usage: i64 = client
        .query_one("SELECT COUNT(*) FROM by_tenant_usage", &[])
        .unwrap()
        .get(0);
    assert_eq!(usage, 0);
    row.get(0)
}

/// A schema as the release before scheduling made it: every table, the
/// `requires` column and the workers table, but no priority, tenant or
/// enqueue time on the queue and no tenants' usage; with one operation
/// queued.
fn previous_release(url: &str) {
    PostgresStore::open(url).unwrap();
    let mut client = postgres::Client::connect(url, postgres::NoTls).unwrap();
    client
        .batch_execute(
            "ALTER TABLE by_operation_queue DROP COLUMN priority, DROP COLUMN tenant, \
                 DROP COLUMN enqueued_ms; \
             DROP TABLE by_tenant_usage; \
             INSERT INTO by_operations (id, body) VALUES ('op-old', '{}'); \
             INSERT INTO by_operation_queue (id, repo, work) VALUES ('op-old', 'app', '{}')",
        )
        .unwrap();
}

/// Servers and workers starting together on one database, on a fresh one
/// and on one whose queue needs the migration, all open the registry.
#[test]
fn registries_opened_at_once_make_and_migrate_the_schema_once() {
    let Some(url) = database() else { return };
    open_together(&url, 8);
    open_together(&url, 8);
    let store = PostgresStore::open(&url).unwrap();
    assert!(store.load().unwrap().is_empty());

    let Some(url) = database() else { return };
    old_queue(&url);
    open_together(&url, 8);
    assert!(requires_of_old(&url).is_empty());

    let Some(url) = database() else { return };
    previous_release(&url);
    open_together(&url, 8);
    assert!(requires_of_old(&url).is_empty());
}

/// A registry opened while another server is between the two writes of
/// its `start` or `finish` (the queue row, then the operation) neither
/// deadlocks with it nor fails it. Opening used to run the whole schema
/// in one transaction: `CREATE UNIQUE INDEX IF NOT EXISTS` held a `SHARE`
/// lock on `by_operations` while `CREATE INDEX IF NOT EXISTS` waited for
/// one on the queue, held by the other server, which waited for the first
/// lock to write the operation.
#[test]
fn a_registry_opened_beside_a_working_server_does_not_deadlock_with_it() {
    for (old, previous) in [(false, false), (true, false), (true, true)] {
        let Some(url) = database() else { return };
        if previous {
            previous_release(&url);
        } else if old {
            old_queue(&url);
        } else {
            PostgresStore::open(&url).unwrap();
            let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
            client
                .batch_execute(
                    "INSERT INTO by_operations (id, body) VALUES ('op-old', '{}'); \
                     INSERT INTO by_operation_queue (id, repo, work) VALUES ('op-old', 'app', '{}')",
                )
                .unwrap();
        }
        let mut working = postgres::Client::connect(&url, postgres::NoTls).unwrap();
        let mut tx = working.transaction().unwrap();
        tx.execute(
            "UPDATE by_operation_queue SET attempt = attempt + 1 WHERE id = 'op-old'",
            &[],
        )
        .unwrap();
        std::thread::scope(|scope| {
            let opening = scope.spawn(|| PostgresStore::open(&url).map(drop));
            // Long enough for the open to reach the queue.
            std::thread::sleep(Duration::from_millis(500));
            tx.execute(
                "UPDATE by_operations SET body = '{\"done\":true}' WHERE id = 'op-old'",
                &[],
            )
            .expect("the working server's write");
            tx.commit().expect("the working server's commit");
            opening.join().unwrap().expect("the registry opens");
        });
        if old {
            assert!(requires_of_old(&url).is_empty());
        }
    }
}

/// The scheduling conformance on PostgreSQL (priority, weighted fair share,
/// aging, two workers racing through the single-statement claim, and the
/// model check over random submissions), on a queue made by the release
/// before scheduling and migrated when opened.
#[test]
fn scheduling_conforms_on_postgres() {
    let Some(url) = database() else { return };
    previous_release(&url);
    let store = PostgresStore::open(&url).unwrap();
    // The old row is the default tenant's; it is claimed like any other.
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    client
        .batch_execute("DELETE FROM by_operation_queue; DELETE FROM by_operations")
        .unwrap();
    branchyard_server::store::check_scheduling(&store, "pg");
}

/// Workers in other processes' threads, each with its own connection,
/// claiming from one queue at once through the scheduling claim: each
/// operation once, and the tenants' usage counts every claim.
#[test]
fn concurrent_scheduled_claims_on_separate_connections_never_double_claim() {
    use branchyard_server::store::{Scheduling, Worker};
    let Some(url) = database() else { return };
    let admitting = PostgresStore::open(&url).unwrap();
    for n in 0..60 {
        let mut op = stored(
            &format!("op-{n}"),
            &format!("t{}", n % 3),
            &format!("b{n}"),
            &[],
        );
        op.operation.priority = n % 7 - 3;
        op.operation.created_at_ms = 1_000 + n as u64;
        assert_eq!(
            admitting
                .admit(&op, &serde_json::json!({}), &AdmissionQuota::default())
                .unwrap(),
            branchyard_server::store::Admission::Admitted
        );
    }
    let scheduling = Scheduling {
        weights: [("t0".to_owned(), 2.0)].into(),
        ..Scheduling::default()
    };
    let mut won: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|w| {
                let (url, scheduling) = (url.clone(), scheduling.clone());
                scope.spawn(move || {
                    let store = PostgresStore::open(&url).unwrap();
                    let worker = Worker {
                        id: format!("racer-{w}"),
                        ..Worker::current()
                    };
                    let mut got = Vec::new();
                    while let Some(claim) = store
                        .claim_next(
                            &worker,
                            &["app".to_owned()],
                            &[],
                            Duration::from_secs(300),
                            &scheduling,
                        )
                        .unwrap()
                    {
                        got.push(claim.operation.operation.id.clone());
                    }
                    got
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    won.sort();
    let mut expected: Vec<String> = (0..60).map(|n| format!("op-{n}")).collect();
    expected.sort();
    assert_eq!(won, expected, "each operation claimed exactly once");
    let mut client = postgres::Client::connect(&url, postgres::NoTls).unwrap();
    let used: f64 = client
        .query_one("SELECT SUM(used) FROM by_tenant_usage", &[])
        .unwrap()
        .get(0);
    assert!((used - 60.0).abs() < 0.5, "{used}");
    assert!(admitting.queue().unwrap().iter().all(|q| q.claimed));
}
