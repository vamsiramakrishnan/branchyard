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
use branchyard_server::ops::{NewOperation, Options, Registry, WORKER_LOST};
use branchyard_server::store::{OperationStore, PostgresStore, Worker};
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
    let first = store.claim(&ghost, &repos, lease).unwrap().unwrap();
    let second = store.claim(&ghost, &repos, lease).unwrap().unwrap();
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
