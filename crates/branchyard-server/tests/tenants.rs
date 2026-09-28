//! Tenant identity, scopes and quotas over real HTTP: see
//! `docs/server.md#identity-and-scopes` and `docs/server.md#quotas`.

mod common;

use branchyard_client::api::{MergeRequest, OperationState};
use branchyard_client::new_key;
use branchyard_server::config::{Credential, Principal, TenantPolicy, Token};
use common::{eventually, raw, run, started, task, wait, Fixture, Server, TOKEN};

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body:?}"))
}

/// A single-token config keeps working exactly as before: one principal,
/// the unconfigured `default` tenant, every scope, every repository.
#[test]
fn single_token_configs_keep_working_unchanged() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();
    assert_eq!(client.repos().unwrap()[0].name, "app");
    let op = run(&client, &task("WRITE a.txt=x", "one"));
    assert_eq!(op.state, OperationState::Succeeded);
    // Merging (needs `merge`) and removing (needs `admin`) both work: the
    // default principal for a bare `tokens` entry has every scope.
    let merged = client
        .repo("app")
        .merge("one", &MergeRequest::default(), &new_key())
        .unwrap();
    assert_eq!(wait(&client, &merged.id).state, OperationState::Succeeded);
}

/// Every endpoint checks the principal's scope: `read` for lookups, `run`
/// for starting or acting on a turn, `merge` for merging/integrating,
/// `admin` for removing a branch.
#[test]
fn scopes_are_enforced_per_endpoint() {
    let f = Fixture::new();
    let mut config = f.config();
    config.tokens = vec![
        Token {
            name: "reader".into(),
            secret: "reader-token-0123456789".into(),
        },
        Token {
            name: "runner".into(),
            secret: "runner-token-0123456789".into(),
        },
    ];
    config.principals.insert(
        "reader".into(),
        Principal {
            name: "reader".into(),
            tenant: "default".into(),
            scopes: ["read"].into_iter().map(String::from).collect(),
            repos: None,
        },
    );
    config.principals.insert(
        "runner".into(),
        Principal {
            name: "runner".into(),
            tenant: "default".into(),
            scopes: ["read", "run"].into_iter().map(String::from).collect(),
            repos: None,
        },
    );
    let server = Server::start(config);
    let reader = branchyard_client::Client::new(&server.url(), "reader-token-0123456789").unwrap();
    let runner = branchyard_client::Client::new(&server.url(), "runner-token-0123456789").unwrap();

    // `read` alone: repos and branches list, but not submitting a task.
    assert_eq!(reader.repos().unwrap()[0].name, "app");
    let denied = reader
        .repo("app")
        .submit_task(&task("WRITE a.txt=x", "r1"), &new_key())
        .unwrap_err();
    assert_eq!(denied.code(), Some("scope_required"));

    // `run` (but not `merge`): can submit and act on a turn, not merge.
    let op = run(&runner, &task("WRITE a.txt=x", "r2"));
    assert_eq!(op.state, OperationState::Succeeded);
    let denied = runner
        .repo("app")
        .merge("r2", &MergeRequest::default(), &new_key())
        .unwrap_err();
    assert_eq!(denied.code(), Some("scope_required"));

    // Not `admin`: cannot remove a branch either.
    let denied = runner.repo("app").remove("r2").unwrap_err();
    assert_eq!(denied.code(), Some("scope_required"));

    // The detail names the missing scope.
    let (status, _, body) = raw(
        server.addr,
        &common::post(
            "/v1/repos/app/branches/r2/merge",
            Some("runner-token-0123456789"),
            "",
            "{}",
        ),
    );
    assert_eq!(status, 403);
    assert_eq!(json(&body)["error"]["detail"]["scope"], "merge");
}

/// Repositories belong to tenants (a tenant's `TenantPolicy.repos`); a
/// principal's own allowlist, if given, only narrows that further.
/// Cross-tenant isolation: a principal of one tenant cannot see or act on
/// another tenant's repository, branches or operations, even when both are
/// served by the same process.
#[test]
fn cross_tenant_isolation() {
    let f = Fixture::new();
    let repo_b = f.extra_repo("appb");
    let mut config = f.config();
    config.repos.push(("appb".into(), repo_b));
    config.tokens = vec![
        Token {
            name: "acme".into(),
            secret: "acme-token-0123456789ab".into(),
        },
        Token {
            name: "globex".into(),
            secret: "globex-token-0123456789".into(),
        },
    ];
    config.principals.insert(
        "acme".into(),
        Principal {
            name: "acme".into(),
            tenant: "acme".into(),
            scopes: ["read", "run", "merge", "admin"]
                .into_iter()
                .map(String::from)
                .collect(),
            repos: None,
        },
    );
    config.principals.insert(
        "globex".into(),
        Principal {
            name: "globex".into(),
            tenant: "globex".into(),
            scopes: ["read", "run", "merge", "admin"]
                .into_iter()
                .map(String::from)
                .collect(),
            repos: None,
        },
    );
    config.tenants.insert(
        "acme".into(),
        TenantPolicy {
            repos: Some(["app".to_owned()].into_iter().collect()),
            ..TenantPolicy::default()
        },
    );
    config.tenants.insert(
        "globex".into(),
        TenantPolicy {
            repos: Some(["appb".to_owned()].into_iter().collect()),
            ..TenantPolicy::default()
        },
    );
    let server = Server::start(config);
    let acme = branchyard_client::Client::new(&server.url(), "acme-token-0123456789ab").unwrap();
    let globex = branchyard_client::Client::new(&server.url(), "globex-token-0123456789").unwrap();

    // Each tenant sees only its own repository.
    assert_eq!(
        acme.repos()
            .unwrap()
            .iter()
            .map(|r| r.name.clone())
            .collect::<Vec<_>>(),
        ["app"]
    );
    assert_eq!(
        globex
            .repos()
            .unwrap()
            .iter()
            .map(|r| r.name.clone())
            .collect::<Vec<_>>(),
        ["appb"]
    );

    // Acting on the other tenant's repository is refused outright.
    let denied = globex
        .repo("app")
        .submit_task(&task("WRITE a.txt=x", "x"), &new_key())
        .unwrap_err();
    assert_eq!(denied.code(), Some("repo_not_allowed"));

    // An operation is invisible to another tenant: it reads as unknown,
    // exactly like an ID that never existed, never as forbidden.
    let op = run(&acme, &task("WRITE a.txt=x", "acme-branch"));
    let hidden = globex.operation(&op.id).unwrap_err();
    assert_eq!(hidden.code(), Some("unknown_operation"));
    // Its own tenant can still read it.
    assert_eq!(acme.operation(&op.id).unwrap().id, op.id);
}

/// `max_running`, reserved atomically at admission (so two requests racing
/// past the limit cannot both start) and released the moment the turn
/// ends.
#[test]
fn max_running_quota_is_reserved_and_released() {
    let f = Fixture::new();
    let mut config = f.config();
    config.tenants.insert(
        "default".into(),
        TenantPolicy {
            max_running: Some(1),
            ..TenantPolicy::default()
        },
    );
    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");
    let op = repo.submit_task(&task("HANG", "held"), &new_key()).unwrap();
    eventually("the prompt to be submitted", || {
        repo.events("held", 0).is_ok_and(|page| {
            page.events
                .iter()
                .any(|e| matches!(e.activity, branchyard::Activity::Prompt(_)))
        })
    });
    let denied = repo
        .submit_task(&task("WRITE a.txt=x", "second"), &new_key())
        .unwrap_err();
    assert_eq!(denied.code(), Some("quota_exceeded"));
    assert_eq!(repo.cancel("held").unwrap(), ["held"]);
    assert_eq!(wait(&client, &op.id).state, OperationState::Succeeded);
    // Released: a new one is admitted now.
    let after = run(&client, &task("WRITE a.txt=x", "third"));
    assert_eq!(after.state, OperationState::Succeeded);
}

/// `max_running` counts the tenant's queued and running operations in the
/// operation store, so it holds across a restart: an operation interrupted
/// at shutdown frees its slot, and one still queued keeps holding its own
/// until it finishes on the restarted server.
#[test]
fn max_running_quota_holds_across_a_restart_with_queued_operations() {
    let f = Fixture::new();
    let mut config = f.config();
    // One turn at a time in this process, two per tenant: a second
    // operation of the tenant waits queued.
    config.max_running = 1;
    config.shutdown_grace = std::time::Duration::from_millis(300);
    config.tenants.insert(
        "default".into(),
        TenantPolicy {
            max_running: Some(2),
            ..TenantPolicy::default()
        },
    );
    let server = Server::start(config.clone());
    let client = server.client();
    let repo = client.repo("app");
    let first = repo.submit_task(&task("HANG", "h1"), &new_key()).unwrap();
    eventually("h1 to start", || started(&client, "app", "h1"));
    let queued = repo.submit_task(&task("HANG", "h2"), &new_key()).unwrap();
    assert_eq!(queued.state, OperationState::Queued);
    let denied = repo
        .submit_task(&task("WRITE a.txt=x", "h3"), &new_key())
        .unwrap_err();
    assert_eq!(denied.code(), Some("quota_exceeded"));
    server.stop();

    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");
    assert_eq!(
        client.operation(&first.id).unwrap().state,
        OperationState::Interrupted
    );
    // The queued one survived the restart and runs now; it still counts.
    eventually("h2 to start", || started(&client, "app", "h2"));
    let third = repo
        .submit_task(&task("WRITE a.txt=x", "h3"), &new_key())
        .unwrap();
    let denied = repo
        .submit_task(&task("WRITE a.txt=y", "h4"), &new_key())
        .unwrap_err();
    assert_eq!(denied.code(), Some("quota_exceeded"));
    assert!(denied.to_string().contains("(2 of 2)"), "{denied}");
    assert_eq!(repo.cancel("h2").unwrap(), ["h2"]);
    assert_eq!(wait(&client, &queued.id).state, OperationState::Succeeded);
    assert_eq!(wait(&client, &third.id).state, OperationState::Succeeded);
    let after = run(&client, &task("WRITE a.txt=y", "h4"));
    assert_eq!(after.state, OperationState::Succeeded);
}

/// `max_branches`, checked in the admission's transaction against the
/// branches the tenant's repositories hold and those its unfinished
/// operations will create: refused once that many exist, available again
/// once one is removed.
#[test]
fn max_branches_quota_is_enforced_live() {
    let f = Fixture::new();
    let mut config = f.config();
    config.tenants.insert(
        "default".into(),
        TenantPolicy {
            max_branches: Some(1),
            ..TenantPolicy::default()
        },
    );
    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");
    let first = run(&client, &task("WRITE a.txt=x", "b1"));
    assert_eq!(first.state, OperationState::Succeeded);
    let denied = repo
        .submit_task(&task("WRITE a.txt=y", "b2"), &new_key())
        .unwrap_err();
    assert_eq!(denied.code(), Some("quota_exceeded"));
    assert!(denied.to_string().contains("max_branches"));
    repo.remove("b1").unwrap();
    let second = run(&client, &task("WRITE a.txt=y", "b2"));
    assert_eq!(second.state, OperationState::Succeeded);
}

/// A bearer token's hash, not its plaintext, is what the server compares
/// and what a `credentials` entry stores; "rotating" it (dropping the old
/// hash for a new one) invalidates the old token immediately.
#[test]
fn tokens_are_verified_by_hash_and_rotate_by_replacing_the_credential() {
    let f = Fixture::new();
    let mut config = f.config();
    config.tokens.clear();
    let old_secret = "old-secret-0123456789ab";
    let new_secret = "new-secret-0123456789ab";
    config.credentials = vec![Credential {
        token_sha256: branchyard_server::config::sha256_hex(old_secret.as_bytes()),
        principal: Principal::default_for("ops"),
    }];
    let server = Server::start(config.clone());
    let client = branchyard_client::Client::new(&server.url(), old_secret).unwrap();
    assert_eq!(client.repos().unwrap()[0].name, "app");
    server.stop();

    // Rotate: the old hash is gone, a new one takes its place. The
    // plaintext of either token never appears in the configuration.
    config.credentials = vec![Credential {
        token_sha256: branchyard_server::config::sha256_hex(new_secret.as_bytes()),
        principal: Principal::default_for("ops"),
    }];
    assert!(!format!("{config:?}").contains(old_secret));
    assert!(!format!("{config:?}").contains(new_secret));
    let server = Server::start(config);
    let old_client = branchyard_client::Client::new(&server.url(), old_secret).unwrap();
    assert_eq!(old_client.repos().unwrap_err().code(), Some("unauthorized"));
    let new_client = branchyard_client::Client::new(&server.url(), new_secret).unwrap();
    assert_eq!(new_client.repos().unwrap()[0].name, "app");
}

/// `credentials` and `tokens` (with a `principals` override) both work,
/// side by side, and a hash's principal is exactly what was configured.
#[test]
fn credentials_and_scoped_tokens_compose() {
    let f = Fixture::new();
    let mut config = f.config();
    let secret = "cred-secret-0123456789ab";
    config.credentials.push(Credential {
        token_sha256: branchyard_server::config::sha256_hex(secret.as_bytes()),
        principal: Principal {
            name: "svc".into(),
            tenant: "svc-tenant".into(),
            scopes: ["read"].into_iter().map(String::from).collect(),
            repos: None,
        },
    });
    let server = Server::start(config);
    let both = branchyard_client::Client::new(&server.url(), TOKEN).unwrap();
    assert_eq!(both.repos().unwrap()[0].name, "app");
    let svc = branchyard_client::Client::new(&server.url(), secret).unwrap();
    assert_eq!(svc.repos().unwrap()[0].name, "app");
    let denied = svc
        .repo("app")
        .submit_task(&task("WRITE a.txt=x", "svc1"), &new_key())
        .unwrap_err();
    assert_eq!(denied.code(), Some("scope_required"));
}

/// Idempotency keys are scoped to the tenant as well as the subject: two
/// tenants' principals with one name, sending one key, get operations of
/// their own, and neither can find the other's by that key.
#[test]
fn idempotency_keys_are_scoped_per_tenant() {
    let f = Fixture::new();
    let appb = f.extra_repo("appb");
    let mut config = f.config();
    common::two_tenants(&mut config, appb, None);
    // Both principals are named `ci`, in different tenants.
    for (token, tenant) in [
        (common::ACME_TOKEN, "acme"),
        (common::GLOBEX_TOKEN, "globex"),
    ] {
        let mut principal = Principal::default_for("ci");
        principal.tenant = tenant.into();
        config.tokens.retain(|t| t.secret != token);
        config.principals.remove(tenant);
        config.credentials.push(Credential {
            token_sha256: branchyard_server::config::sha256_hex(token.as_bytes()),
            principal,
        });
    }
    let server = Server::start(config);
    let acme = branchyard_client::Client::new(&server.url(), common::ACME_TOKEN).unwrap();
    let globex = branchyard_client::Client::new(&server.url(), common::GLOBEX_TOKEN).unwrap();
    let key = new_key();
    let a = acme
        .repo("app")
        .submit_task(&task("WRITE a.txt=a", "same"), &key)
        .unwrap();
    let b = globex
        .repo("appb")
        .submit_task(&task("WRITE a.txt=a", "same"), &key)
        .unwrap();
    assert_ne!(a.id, b.id);
    assert_eq!(acme.operation_by_key(&key).unwrap().id, a.id);
    assert_eq!(globex.operation_by_key(&key).unwrap().id, b.id);
    assert_eq!(wait(&acme, &a.id).state, OperationState::Succeeded);
    assert_eq!(wait(&globex, &b.id).state, OperationState::Succeeded);
}

/// A graph proposal is not an operation, but its spawns are branches:
/// `max_branches` refuses a proposal that would take the tenant past it,
/// with nothing applied, and admits one that fits.
#[test]
fn max_branches_quota_covers_graph_proposals() {
    use branchyard::{Envelope, GraphEdit, SpawnSpec};
    use branchyard_client::api::{GraphRequest, PolicySpec, TaskRequest};
    let f = Fixture::new();
    let mut config = f.config();
    config.by_path = Some("/bin/true".into());
    config.allow_delegation = true;
    config.tenants.insert(
        "default".into(),
        TenantPolicy {
            max_branches: Some(2),
            ..TenantPolicy::default()
        },
    );
    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");
    let root = run(
        &client,
        &TaskRequest {
            delegation: Some(Envelope::default()),
            ..task("say hi", "root")
        },
    );
    assert_eq!(root.state, OperationState::Succeeded, "{root:?}");
    let spawn = |name: &str| {
        GraphEdit::Spawn(SpawnSpec {
            prompt: "WRITE f.txt=1".into(),
            name: Some(name.into()),
            ..SpawnSpec::default()
        })
    };
    let request = |edits| GraphRequest {
        expected_revision: 0,
        edits,
        policy: PolicySpec::allow_all(),
        unapproved_tools: false,
    };
    let denied = repo
        .apply_graph("root", &request(vec![spawn("one"), spawn("two")]))
        .unwrap_err();
    assert_eq!(denied.code(), Some("quota_exceeded"), "{denied:?}");
    assert!(denied.to_string().contains("max_branches"), "{denied}");
    assert_eq!(repo.graph("root").unwrap().revision, 0);
    assert_eq!(repo.branches().unwrap().len(), 1);
    let applied = repo
        .apply_graph("root", &request(vec![spawn("one")]))
        .unwrap();
    assert_eq!(applied.revision, 1);
}

/// Every read needs the `read` scope, whatever else a credential holds:
/// listing repositories, reading an operation (by ID or by key) and a
/// scratch area's lock are all refused without it.
#[test]
fn every_read_needs_the_read_scope() {
    let f = Fixture::new();
    let mut config = f.config();
    config.tokens.push(Token {
        name: "blind".into(),
        secret: "blind-token-0123456789".into(),
    });
    config.principals.insert(
        "blind".into(),
        Principal {
            name: "blind".into(),
            tenant: "default".into(),
            scopes: ["run", "merge", "admin"]
                .into_iter()
                .map(String::from)
                .collect(),
            repos: None,
        },
    );
    let server = Server::start(config);
    let op = run(&server.client(), &task("WRITE a.txt=x", "r1"));
    assert_eq!(op.state, OperationState::Succeeded);
    for path in [
        "/v1/repos".to_owned(),
        "/v1/harnesses".to_owned(),
        format!("/v1/operations/{}", op.id),
        "/v1/operations?idempotency_key=k".to_owned(),
        "/v1/repos/app/scratch/notes/lock".to_owned(),
        "/v1/repos/app/branches".to_owned(),
    ] {
        let (status, _, body) = raw(
            server.addr,
            &common::get(&path, Some("blind-token-0123456789")),
        );
        assert_eq!(status, 403, "{path}: {body}");
        assert_eq!(json(&body)["error"]["detail"]["scope"], "read", "{path}");
    }
    // Writing is still admitted: the scope check is for reads only (and
    // following the operation would need `read`).
    let blind = branchyard_client::Client::new(&server.url(), "blind-token-0123456789").unwrap();
    blind
        .repo("app")
        .submit_task(&task("WRITE b.txt=x", "r2"), &new_key())
        .unwrap();
}

/// `max_artifact_bytes` also holds when artifact bytes are added: an
/// upload that would take the tenant past it is refused, and one that
/// fits is published.
#[test]
fn max_artifact_bytes_covers_uploads() {
    let f = Fixture::new();
    let mut config = f.config();
    config.tenants.insert(
        "default".into(),
        TenantPolicy {
            max_artifact_bytes: Some(10),
            ..TenantPolicy::default()
        },
    );
    let server = Server::start(config);
    let client = server.client();
    let op = run(&client, &task("WRITE a.txt=x", "b1"));
    assert_eq!(op.state, OperationState::Succeeded);
    let repo = client.repo("app");
    repo.publish_artifact("b1", b"123456", Some("first"), None, &[], &new_key())
        .unwrap();
    let denied = repo
        .publish_artifact("b1", b"abcdef", Some("second"), None, &[], &new_key())
        .unwrap_err();
    assert_eq!(denied.code(), Some("quota_exceeded"), "{denied:?}");
    assert!(
        denied.to_string().contains("max_artifact_bytes"),
        "{denied}"
    );
    repo.publish_artifact("b1", b"abcd", Some("fits"), None, &[], &new_key())
        .unwrap();
}
