//! What the server's operator must opt into (providers, delegation,
//! unapproved tools), and the delegation endpoints a person uses, over real
//! HTTP on 127.0.0.1 with the fake ACP agent as `gemini-cli`.

mod common;

use branchyard::{BranchStatus, Envelope, Provider, SubstrateOptions, TaskOptions, Yard};
use branchyard_client::api::{
    ForkRequest, OperationKind, OperationState, PolicySpec, SendRequest, SpawnRequest, TaskRequest,
};
use branchyard_client::{new_key, Client};
use common::{get, post, raw, run, task, wait, Fixture, Server, TOKEN};
use serde_json::Value;

fn json(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body:?}"))
}

/// The error code of a refused request.
fn refused(client: &Client, request: &TaskRequest) -> (u16, String, String) {
    match client.repo("app").submit_task(request, &new_key()) {
        Err(branchyard_client::Error::Api { status, error }) => (status, error.code, error.message),
        other => panic!("accepted: {other:?}"),
    }
}

fn substrate(key: &str) -> Provider {
    Provider::Substrate(SubstrateOptions {
        endpoint: "http://127.0.0.1:1".into(),
        router: "http://127.0.0.1:1/{atespace}/{actor}".into(),
        template: "t".into(),
        key: key.into(),
        ..SubstrateOptions::default()
    })
}

#[test]
fn opt_ins_are_refused_unless_the_operator_allows_them() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();

    let (status, code, message) = refused(
        &client,
        &TaskRequest {
            provider: Some(substrate("/k")),
            ..task("x", "sub")
        },
    );
    assert_eq!((status, code.as_str()), (403, "provider_not_allowed"));
    assert!(message.contains("--allow-provider substrate"), "{message}");
    for (request, code) in [
        (
            TaskRequest {
                delegation: Some(Envelope::default()),
                ..task("x", "d")
            },
            "delegation_not_allowed",
        ),
        (
            TaskRequest {
                allow_delegation: true,
                ..task("x", "d")
            },
            "delegation_not_allowed",
        ),
        (
            TaskRequest {
                unapproved_tools: true,
                ..task("x", "u")
            },
            "unapproved_tools_not_allowed",
        ),
    ] {
        let (status, got, _) = refused(&client, &request);
        assert_eq!((status, got.as_str()), (403, code), "{request:?}");
    }

    // The local provider needs no opt-in.
    let op = run(
        &client,
        &TaskRequest {
            provider: Some(Provider::Local),
            ..task("WRITE a.txt=two", "plain")
        },
    );
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    let repo = client.repo("app");
    for (path, body, code) in [
        (
            "/v1/repos/app/branches/plain/fork",
            serde_json::to_string(&ForkRequest {
                prompt: "x".into(),
                provider: Some(substrate("/k")),
                ..ForkRequest::default()
            })
            .unwrap(),
            "provider_not_allowed",
        ),
        (
            "/v1/repos/app/branches/plain/send",
            serde_json::to_string(&SendRequest {
                prompt: "x".into(),
                delegation: Some(Envelope::default()),
                ..SendRequest::default()
            })
            .unwrap(),
            "delegation_not_allowed",
        ),
        (
            "/v1/repos/app/branches/plain/spawn",
            r#"{"prompt": "x"}"#.to_owned(),
            "delegation_not_allowed",
        ),
    ] {
        let (status, _, answer) = raw(server.addr, &post(path, Some(TOKEN), "", &body));
        assert_eq!(status, 403, "{path}: {answer}");
        assert_eq!(json(&answer)["error"]["code"], code, "{path}");
    }

    // Reading a branch as a person needs no opt-in; integrating one that
    // no branch delegated is refused as it is locally.
    let inspection = repo.inspect("plain").unwrap();
    assert_eq!(inspection.envelope, None);
    assert_eq!(inspection.status, BranchStatus::Ready);
    assert_eq!(repo.children("plain").unwrap().descendants, []);
    let page = repo.event_page("plain", Some(0), 2).unwrap();
    assert_eq!((page.events.len(), page.next_cursor), (2, 2));
    let error = repo.integrate("plain", &new_key()).unwrap_err();
    assert_eq!(error.code(), Some("denied"));
    assert_eq!(
        error.to_string(),
        "denied: plain was not delegated by another branch; merge it with by merge"
    );
    let (status, _, _) = raw(
        server.addr,
        &get(
            "/v1/repos/app/branches/plain/event-page?limit=x",
            Some(TOKEN),
        ),
    );
    assert_eq!(status, 400);
}

#[test]
fn a_substrate_key_must_be_a_server_path() {
    let f = Fixture::new();
    let mut config = f.config();
    config.allow_providers.insert("substrate".into());
    let server = Server::start(config);
    let (status, code, message) = refused(
        &server.client(),
        &TaskRequest {
            provider: Some(substrate("bridge.key")),
            ..task("x", "sub")
        },
    );
    assert_eq!((status, code.as_str()), (400, "invalid_request"));
    assert!(message.contains("absolute path on the server"), "{message}");
}

#[test]
fn a_person_spawns_inspects_and_integrates_through_the_server() {
    let f = Fixture::new();
    let mut config = f.config();
    config.allow_delegation = true;
    // The fake agent starts the MCP server only when asked to, so any
    // executable stands in for `by` here; `by --remote` tests run the real
    // one.
    config.by_path = Some("/bin/true".into());
    let server = Server::start(config.clone());
    let client = server.client();
    let repo = client.repo("app");

    let root = run(
        &client,
        &TaskRequest {
            delegation: Some(Envelope::depth(2)),
            ..task("ENV BRANCHYARD_BRANCH BRANCHYARD_BY", "root")
        },
    );
    assert_eq!(root.state, OperationState::Succeeded, "{root:?}");
    let said = repo
        .events("root", 0)
        .unwrap()
        .events
        .iter()
        .filter_map(|e| match &e.activity {
            branchyard::Activity::Harness(branchyard::Event::MessageDelta { text, .. }) => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect::<String>();
    assert!(
        said.contains("BRANCHYARD_BRANCH=root") && said.contains("BRANCHYARD_BY=/bin/true"),
        "the harness got the delegation environment: {said}"
    );

    let op = repo
        .spawn(
            "root",
            &SpawnRequest {
                prompt: "WRITE kid.txt=k".into(),
                name: Some("kid".into()),
                policy: PolicySpec::allow_all(),
                ..SpawnRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    assert_eq!(
        (op.kind, op.branches.as_slice()),
        (OperationKind::Spawn, &["kid".to_owned()][..])
    );
    let op = wait(&client, &op.id);
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    let result = op.result.unwrap();
    let inspection = result.inspection.unwrap();
    assert_eq!(inspection.status, BranchStatus::Ready);
    assert_eq!(inspection.parent.as_deref(), Some("root"));
    assert_eq!(inspection.envelope.as_ref().map(|e| e.max_depth), Some(1));
    assert_eq!(result.branches[0].name, "kid");

    // The same values the SDK gives in the server's repository.
    let yard = Yard::open(&f.root).unwrap();
    let local = yard
        .branch("root")
        .unwrap()
        .delegate(TaskOptions::default())
        .unwrap();
    assert_eq!(repo.inspect("kid").unwrap(), local.inspect("kid").unwrap());
    assert_eq!(repo.children("root").unwrap(), local.children().unwrap());
    assert_eq!(
        repo.event_page("kid", Some(1), 3).unwrap(),
        local.events("kid", Some(1), 3).unwrap()
    );
    assert_eq!(
        repo.event_page("kid", None, 50).unwrap(),
        local.events("kid", None, 50).unwrap()
    );

    let op = wait(&client, &repo.integrate("kid", &new_key()).unwrap().id);
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    assert_eq!(op.kind, OperationKind::Integrate);
    let merged = op.result.unwrap().merged.unwrap();
    assert_eq!(
        (merged.branch.as_str(), merged.target.as_str()),
        ("kid", "by/root")
    );
    assert_eq!(common::git(&f.root, &["show", "by/root:kid.txt"]), "k\n");

    // The envelope binds a person through the server as it does locally.
    let op = wait(
        &client,
        &repo
            .spawn(
                "root",
                &SpawnRequest {
                    prompt: "x".into(),
                    harness: Some("codex".into()),
                    ..SpawnRequest::default()
                },
                &new_key(),
            )
            .unwrap()
            .id,
    );
    assert_eq!(op.state, OperationState::Failed);
    let error = op.error.unwrap();
    assert_eq!(error.code, "denied");
    assert!(
        error.message.contains("may not delegate to codex"),
        "{error:?}"
    );
    drop(server);

    // A branch that was given delegation is not sent to by a server that
    // offers none.
    config.allow_delegation = false;
    let server = Server::start(config);
    let error = server
        .client()
        .repo("app")
        .send(
            "root",
            &SendRequest {
                prompt: "again".into(),
                ..SendRequest::default()
            },
            &new_key(),
        )
        .unwrap_err();
    assert_eq!(error.code(), Some("delegation_not_allowed"));
    assert!(
        error.to_string().contains("root was given delegation"),
        "{error}"
    );
}

#[cfg(not(feature = "postgres"))]
#[test]
fn a_database_needs_a_build_with_postgres() {
    let f = Fixture::new();
    let mut config = f.config();
    config.database = Some("postgres://nobody@127.0.0.1:1/none".into());
    let error = Server::try_start(config).err().unwrap();
    assert!(error.contains("no PostgreSQL support"), "{error}");
}
