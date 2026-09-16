//! Requires a disposable PostgreSQL database with PGMQ 1.13.0 installed. Never silently skipped by CI.
mod common;
use branchyard_protocol::*;
use branchyard_server::{
    config::{Config, Principal},
    http, Error, Store,
};
use common::*;
use sqlx::{PgPool, Row};
use std::time::Duration;

async fn database(
    mut configure: impl FnMut(&mut Config),
) -> (Store, PgPool, Config, Principal, Command) {
    let (mut config, principal, command) = setup();
    configure(&mut config);
    let url = std::env::var("BRANCHYARD_TEST_DATABASE_URL")
        .expect("a disposable PostgreSQL/PGMQ database is required");
    let pool = PgPool::connect(&url).await.unwrap();
    Store::migrate(&pool).await.unwrap();
    let store = Store::connect(&url, config.clone()).await.unwrap();
    store.initialize().await.unwrap();
    (store, pool, config, principal, command)
}
async fn counts(pool: &PgPool, tenant: &str) -> (i64, i64, i64, i64) {
    let row = sqlx::query("SELECT (SELECT count(*) FROM by_operations WHERE tenant=$1) AS ops,(SELECT count(*) FROM by_tasks WHERE tenant=$1) AS tasks,(SELECT count(*) FROM pgmq.q_branchyard_dispatch WHERE message->>'tenant'=$1) AS queue,(SELECT reserved_tasks FROM by_tenants WHERE tenant=$1) AS reserved").bind(tenant).fetch_one(pool).await.unwrap();
    (
        row.get("ops"),
        row.get("tasks"),
        row.get("queue"),
        row.get("reserved"),
    )
}
#[tokio::test]
#[ignore = "requires PostgreSQL and PGMQ; required in database CI"]
async fn concurrent_duplicates_and_restart_preserve_one_admission() {
    let (store, pool, config, principal, command) = database(|_| {}).await;
    let mut pending = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let s = store.clone();
        let p = principal.clone();
        let c = command.clone();
        pending.spawn(async move { s.submit(&p, &c).await.unwrap() });
    }
    let mut accepted = 0;
    while let Some(r) = pending.join_next().await {
        if r.unwrap().disposition == Disposition::Accepted {
            accepted += 1;
        }
    }
    assert_eq!(accepted, 1);
    assert_eq!(counts(&pool, &principal.tenant).await, (1, 1, 1, 1));
    drop(store);
    let restarted = Store::connect(
        &std::env::var("BRANCHYARD_TEST_DATABASE_URL").unwrap(),
        config,
    )
    .await
    .unwrap();
    assert_eq!(
        restarted
            .operation(&principal, command.operation_id)
            .await
            .unwrap()
            .request_sha256,
        command.fingerprint().unwrap()
    );
    assert_eq!(
        restarted
            .submit(&principal, &command)
            .await
            .unwrap()
            .disposition,
        Disposition::Replay
    );
    let mut changed = command.clone();
    if let Action::CreateTask { spec, .. } = &mut changed.action {
        spec.goal = "changed".into();
    }
    assert!(matches!(
        restarted.submit(&principal, &changed).await,
        Err(Error::Conflict)
    ));
}
#[tokio::test]
#[ignore = "requires PostgreSQL and PGMQ; required in database CI"]
async fn stale_graph_cycle_and_quota_leave_no_partial_state() {
    let (store, pool, _, principal, command) = database(|c| {
        for p in c.tenants.values_mut() {
            p.max_tasks_per_root = 2;
        }
    })
    .await;
    store.submit(&principal, &command).await.unwrap();
    let (delta, child) = spawn(&command, 0);
    store.submit(&principal, &delta).await.unwrap();
    let baseline = counts(&pool, &principal.tenant).await;
    let (stale, _) = spawn(&command, 0);
    assert!(matches!(
        store.submit(&principal, &stale).await,
        Err(Error::Conflict)
    ));
    let (over, _) = spawn(&command, 1);
    assert!(matches!(
        store.submit(&principal, &over).await,
        Err(Error::Capacity)
    ));
    let (root_id, _) = root(&command);
    let cycle = Command {
        schema: Version::V1Alpha1,
        operation_id: OperationId::new(),
        action: Action::ApplyGraph {
            root_id,
            expected_revision: 1,
            edits: vec![
                GraphEdit::AddDependency {
                    task_id: root_id,
                    depends_on: child,
                },
                GraphEdit::AddDependency {
                    task_id: child,
                    depends_on: root_id,
                },
            ],
        },
    };
    assert!(matches!(
        store.submit(&principal, &cycle).await,
        Err(Error::Invalid)
    ));
    assert_eq!(counts(&pool, &principal.tenant).await, baseline);
    assert_eq!(
        store
            .task(&principal, root_id)
            .await
            .unwrap()
            .graph_revision,
        1
    );
}
#[tokio::test]
#[ignore = "requires PostgreSQL and PGMQ; required in database CI"]
async fn descendant_scope_cross_tenant_reads_and_cancel_reservations() {
    let (store, pool, _, principal, command) = database(|_| {}).await;
    store.submit(&principal, &command).await.unwrap();
    let (delta, child) = spawn(&command, 0);
    store.submit(&principal, &delta).await.unwrap();
    let (root_id, _) = root(&command);
    let scoped = Principal {
        subtree: Some(child),
        subject: "child".into(),
        ..principal.clone()
    };
    assert!(matches!(
        store.task(&scoped, root_id).await,
        Err(Error::Forbidden)
    ));
    assert!(matches!(
        store
            .submit(
                &scoped,
                &Command {
                    operation_id: OperationId::new(),
                    ..command.clone()
                }
            )
            .await,
        Err(Error::Forbidden)
    ));
    let foreign = Principal {
        tenant: "other-tenant".into(),
        ..principal.clone()
    };
    assert!(matches!(
        store.task(&foreign, child).await,
        Err(Error::NotFound)
    ));
    let cancel = Command {
        schema: Version::V1Alpha1,
        operation_id: OperationId::new(),
        action: Action::CancelTask {
            task_id: child,
            expected_revision: 1,
            cascade: true,
        },
    };
    store.submit(&scoped, &cancel).await.unwrap();
    assert_eq!(
        store.task(&scoped, child).await.unwrap().state,
        TaskState::CancelRequested
    );
    assert_eq!(counts(&pool, &principal.tenant).await.3, 2);
    let page = store.events(&scoped, child, 0, 1).await.unwrap();
    assert_eq!(page.next_after, 1);
    let page = store
        .events(&scoped, child, page.next_after, 1)
        .await
        .unwrap();
    assert_eq!(page.next_after, 2);
    assert_eq!(
        store.events(&scoped, child, 2, 1).await.unwrap().next_after,
        2
    );
}
#[tokio::test]
#[ignore = "requires PostgreSQL and PGMQ; required in database CI"]
async fn queue_failure_rolls_back_graph_reservation_operation_and_events() {
    let (store, pool, _, principal, command) = database(|_| {}).await;
    // Inject a failure at the final queue insertion, scoped to this test's tenant.
    let name = format!("reject_{}", principal.tenant);
    let ddl=format!("CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.message->>'tenant' = '{}' THEN RAISE EXCEPTION 'injected queue failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER {name} BEFORE INSERT ON pgmq.q_branchyard_dispatch FOR EACH ROW EXECUTE FUNCTION {name}();",principal.tenant);
    sqlx::raw_sql(&ddl).execute(&pool).await.unwrap();
    assert!(matches!(
        store.submit(&principal, &command).await,
        Err(Error::Database(_))
    ));
    assert_eq!(counts(&pool, &principal.tenant).await, (0, 0, 0, 0));
    sqlx::raw_sql(&format!(
        "DROP TRIGGER {name} ON pgmq.q_branchyard_dispatch; DROP FUNCTION {name}();"
    ))
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        store
            .submit(&principal, &command)
            .await
            .unwrap()
            .disposition,
        Disposition::Accepted
    );
}
#[tokio::test]
#[ignore = "requires PostgreSQL and PGMQ; required in database CI"]
async fn actual_sdk_reconciles_a_response_lost_after_database_commit() {
    let (store, pool, _, principal, command) = database(|_| {}).await;
    let app = http::router(store).layer(axum::middleware::from_fn(
        |request: axum::extract::Request, next: axum::middleware::Next| async move {
            let delay = request.uri().path().ends_with("/commands");
            let response = next.run(request).await;
            if delay {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            response
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = branchyard_sdk::Client::with_options(
        &url,
        TOKEN,
        branchyard_sdk::ClientOptions {
            timeout: Duration::from_millis(500),
            allow_loopback_http: true,
        },
    )
    .unwrap();
    assert!(!client.info().await.unwrap().execution_ready);
    assert!(matches!(
        client.submit(&command).await,
        Err(branchyard_sdk::Error::SubmissionUnknown { .. })
    ));
    assert_eq!(
        client.reconcile(&command).await.unwrap().state,
        OperationState::Succeeded
    );
    assert_eq!(counts(&pool, &principal.tenant).await, (1, 1, 1, 1));
    server.abort();
    let _ = server.await;
}
#[tokio::test]
#[ignore = "requires PostgreSQL and PGMQ; required in database CI"]
async fn aggregate_root_budget_and_global_task_identity_are_enforced() {
    let (store, pool, _, principal, command) = database(|c| {
        for p in c.tenants.values_mut() {
            p.max_root_cpu_millis = 2000;
        }
    })
    .await;
    store.submit(&principal, &command).await.unwrap();
    let (delta, _) = spawn(&command, 0);
    assert!(matches!(
        store.submit(&principal, &delta).await,
        Err(Error::Capacity)
    ));
    let mut duplicate = command.clone();
    duplicate.operation_id = OperationId::new();
    assert!(matches!(
        store.submit(&principal, &duplicate).await,
        Err(Error::Conflict)
    ));
    assert_eq!(counts(&pool, &principal.tenant).await, (1, 1, 1, 1));
}
