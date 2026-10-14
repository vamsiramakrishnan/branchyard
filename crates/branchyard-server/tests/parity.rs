//! What the server's operator must opt into (providers, delegation,
//! unapproved tools), and the delegation endpoints a person uses, over real
//! HTTP on 127.0.0.1 with the fake ACP agent as `gemini-cli`.

#![allow(clippy::panic)] // tests: a panic is the failure report
mod common;

use std::time::Duration;

use branchyard::{
    BranchStatus, CheckVerdict, Envelope, Provider, RecipeOptions, ReportedState, ResultContract,
    SubstrateOptions, TaskOptions, Yard,
};
use branchyard_client::api::{
    BudgetSpec, ForkRequest, OperationKind, OperationState, PolicySpec, SendRequest, SpawnRequest,
    TaskRequest,
};
use branchyard_client::{new_key, Client};
use branchyard_testkit::wait;
use common::{await_operation, get, post, raw, run, task, Fixture, Server, TOKEN};
use serde_json::{json, Value};

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
    // A recipe's commands come in the request: never run, whatever the
    // operator allows (docs/recipes.md).
    let (status, code, message) = refused(
        &client,
        &TaskRequest {
            provider: Some(Provider::Recipe(RecipeOptions {
                name: "devbox".into(),
                create: "touch /tmp/never-run".into(),
                ..RecipeOptions::default()
            })),
            ..task("x", "vm")
        },
    );
    assert_eq!((status, code.as_str()), (403, "provider_not_allowed"));
    assert!(
        message.contains("a server does not run environment recipes"),
        "{message}"
    );
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
    let op = await_operation(&client, &op.id);
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

    // Messaging: the same values and authority the SDK gives locally.
    let reported = repo.report("kid", "tests pass").unwrap();
    assert_eq!(
        (reported.from.as_str(), reported.to.as_str()),
        ("kid", "root")
    );
    assert_eq!(reported.kind, branchyard::MessageKind::Report);
    assert_eq!(repo.inbox("root").unwrap(), local.inbox().unwrap());
    let answer = repo.answer("root", reported.id, "thanks").unwrap();
    assert_eq!((answer.from.as_str(), answer.to.as_str()), ("root", "kid"));
    assert_eq!(answer.in_reply_to, Some(reported.id));
    let asked = repo.ask("kid", "should I rename it?", None).unwrap();
    assert!(asked.answer.is_none());
    let no_parent = repo.report("root", "root has no parent").unwrap_err();
    assert_eq!(no_parent.code(), Some("denied"));

    // Self-reported status: a person states kid's, under the tool's rules.
    let working = repo
        .report_status(
            "kid",
            ReportedState::Working,
            "  reading the tests\n",
            Some("https://example.com/pull/7"),
        )
        .unwrap();
    assert_eq!(working.report.state, ReportedState::Working);
    assert_eq!(working.report.summary, "reading the tests");
    assert_eq!(
        working.report.link.as_deref(),
        Some("https://example.com/pull/7")
    );
    assert!(working.escalation.is_none(), "{working:?}");
    assert_eq!(
        repo.inspect("kid").unwrap().reported,
        Some(working.report.clone()),
        "the inspection route shows what was recorded"
    );
    // `blocked` is escalated to the parent, once.
    let blocked = repo
        .report_status("kid", ReportedState::Blocked, "needs the API key", None)
        .unwrap();
    let escalation = blocked.escalation.clone().unwrap();
    assert_eq!(
        (escalation.from.as_str(), escalation.to.as_str()),
        ("kid", "root")
    );
    assert_eq!(escalation.kind, branchyard::MessageKind::Escalation);
    assert_eq!(escalation.text, "status blocked: needs the API key");
    assert_eq!(
        repo.inspect("kid").unwrap().reported,
        Some(blocked.report.clone())
    );
    assert_eq!(
        local.inspect("kid").unwrap().reported,
        Some(blocked.report.clone()),
        "the same value the SDK reads in the server's repository"
    );
    let repeated = repo
        .report_status("kid", ReportedState::Blocked, "needs the API key", None)
        .unwrap();
    assert!(repeated.escalation.is_none(), "{repeated:?}");
    // A branch with no parent states its status with nobody to tell.
    let alone = repo
        .report_status("root", ReportedState::Failure, "gave up", None)
        .unwrap();
    assert!(alone.escalation.is_none(), "{alone:?}");
    // A field that breaks a rule is 400 invalid_request naming it; nothing
    // is recorded.
    let before = repo.events("kid", 0).unwrap().events.len();
    let report = |body: &str| {
        let (status, _, answer) = raw(
            server.addr,
            &post(
                "/v1/repos/app/branches/kid/report-status",
                Some(TOKEN),
                "",
                body,
            ),
        );
        (status, json(&answer))
    };
    for (body, field) in [
        (r#"{"state": "done", "summary": "x"}"#.to_owned(), "state"),
        (
            r#"{"state": "Working", "summary": "x"}"#.to_owned(),
            "state",
        ),
        (
            r#"{"state": "working", "summary": "  "}"#.to_owned(),
            "summary",
        ),
        (
            format!(
                r#"{{"state": "working", "summary": "{}"}}"#,
                "x".repeat(281)
            ),
            "summary",
        ),
        (
            r#"{"state": "working", "summary": "x", "link": "ftp://example.com/"}"#.to_owned(),
            "link",
        ),
        (
            r#"{"state": "working", "summary": "x", "link": "javascript:alert(1)"}"#.to_owned(),
            "link",
        ),
    ] {
        let (status, answer) = report(&body);
        assert_eq!(status, 400, "{body}: {answer}");
        assert_eq!(answer["error"]["code"], "invalid_request", "{answer}");
        assert_eq!(answer["error"]["detail"]["field"], field, "{answer}");
        assert!(
            answer["error"]["message"].as_str().unwrap().contains(field),
            "the message names the field too: {answer}"
        );
    }
    let (_, answer) = report(r#"{"state": "done", "summary": "x"}"#);
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("working, idle, blocked, complete, failure"),
        "{answer}"
    );
    // A request that is not the shape is refused before the fields.
    for body in [
        r#"{"summary": "x"}"#,
        r#"{"state": "working"}"#,
        r#"{"state": "working", "summary": "x", "bogus": 1}"#,
    ] {
        let (status, answer) = report(body);
        assert_eq!(status, 400, "{body}: {answer}");
        assert_eq!(answer["error"]["code"], "invalid_request");
    }
    assert_eq!(repo.events("kid", 0).unwrap().events.len(), before);
    let unknown = repo
        .report_status("nope", ReportedState::Idle, "x", None)
        .unwrap_err();
    assert_eq!(unknown.code(), Some("unknown_branch"));

    let op = await_operation(&client, &repo.integrate("kid", &new_key()).unwrap().id);
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    assert_eq!(op.kind, OperationKind::Integrate);
    let merged = op.result.unwrap().merged.unwrap();
    assert_eq!(
        (merged.branch.as_str(), merged.target.as_str()),
        ("kid", "by/root")
    );
    assert_eq!(common::git(&f.root, &["show", "by/root:kid.txt"]), "k\n");

    // A spawn locks its parent: a send to it or its removal is refused
    // while the child runs.
    let hang = repo
        .spawn(
            "root",
            &SpawnRequest {
                prompt: "HANG".into(),
                name: Some("hung".into()),
                budget: BudgetSpec {
                    max_seconds: Some(3.0),
                    ..BudgetSpec::default()
                },
                ..SpawnRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    let busy = repo
        .send(
            "root",
            &SendRequest {
                prompt: "x".into(),
                ..SendRequest::default()
            },
            &new_key(),
        )
        .unwrap_err();
    assert_eq!(busy.code(), Some("branch_busy"), "{busy}");
    assert_eq!(repo.remove("root").unwrap_err().code(), Some("branch_busy"));
    let hang = await_operation(&client, &hang.id);
    assert_eq!(hang.state, OperationState::Succeeded, "{hang:?}");
    assert!(matches!(
        repo.branch("hung").unwrap().status,
        BranchStatus::BudgetExceeded { .. }
    ));

    // The envelope binds a person through the server as it does locally.
    let op = await_operation(
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
    // A spawn's model reaches the engine, which refuses it where the
    // profile cannot deliver one: Gemini CLI's ACP profile writes it into
    // its home, and this child would have none of its own.
    let op = await_operation(
        &client,
        &repo
            .spawn(
                "root",
                &SpawnRequest {
                    prompt: "x".into(),
                    model: Some("small".into()),
                    ..SpawnRequest::default()
                },
                &new_key(),
            )
            .unwrap()
            .id,
    );
    assert_eq!(op.state, OperationState::Failed);
    let error = op.error.unwrap();
    assert_eq!(error.code, "unsupported", "{error:?}");
    assert!(
        error
            .message
            .contains("gemini-cli-acp cannot be given a model")
            && error.message.contains("--isolated"),
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

/// `POST …/graph` commits a proposal before it answers; a stale revision
/// is `409 stale_revision`, and a server without delegation refuses to
/// spawn with `403`. The graph read back is the SDK's.
#[test]
fn a_person_applies_a_graph_through_the_server() {
    use branchyard::{GraphEdit, SpawnSpec};
    use branchyard_client::api::GraphRequest;
    let f = Fixture::new();
    let mut config = f.config();
    config.by_path = Some("/bin/true".into());
    config.allow_delegation = true;
    let server = Server::start(config.clone());
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
    let spawn = |name: &str, prompt: &str, depends_on: &[&str]| {
        GraphEdit::Spawn(SpawnSpec {
            prompt: prompt.into(),
            name: Some(name.into()),
            depends_on: depends_on.iter().map(|s| (*s).to_owned()).collect(),
            ..SpawnSpec::default()
        })
    };
    let request = GraphRequest {
        expected_revision: 0,
        edits: vec![
            spawn("first", "WRITE f.txt=1", &[]),
            spawn("second", "WRITE s.txt=1", &["first"]),
        ],
        policy: PolicySpec::allow_all(),
        unapproved_tools: false,
    };
    let applied = repo.apply_graph("root", &request).unwrap();
    assert_eq!(applied.revision, 1);
    assert_eq!(applied.spawned[1].status, BranchStatus::Waiting);
    match repo.apply_graph("root", &request) {
        Err(branchyard_client::Error::Api { status, error }) => {
            assert_eq!((status, error.code.as_str()), (409, "stale_revision"));
            assert_eq!(
                error.detail,
                Some(serde_json::json!({"expected": 0, "actual": 1}))
            );
        }
        other => panic!("{other:?}"),
    }
    wait::until("second to finish", || {
        repo.inspect("second").unwrap().status == BranchStatus::Ready
    });
    let yard = Yard::open(&f.root).unwrap();
    assert_eq!(repo.graph("root").unwrap(), yard.graph("root").unwrap());
    let (status, _, body) = raw(
        server.addr,
        &get("/v1/repos/app/branches/root/graph", Some(TOKEN)),
    );
    assert_eq!(status, 200);
    assert_eq!(json(&body)["dependencies"][0]["prerequisite"], "first");
    let (status, _, body) = raw(
        server.addr,
        &post(
            "/v1/repos/app/branches/root/graph",
            Some(TOKEN),
            "",
            r#"{"expected_revision": 1, "edits": [], "colour": 1}"#,
        ),
    );
    assert_eq!(status, 400, "{body}");
    drop(client);
    drop(server);
    // Without delegation, a proposal that spawns is refused.
    config.allow_delegation = false;
    let closed = Server::start(config);
    let request = GraphRequest {
        expected_revision: 1,
        ..request
    };
    match closed.client().repo("app").apply_graph("root", &request) {
        Err(branchyard_client::Error::Api { status, error }) => {
            assert_eq!(
                (status, error.code.as_str()),
                (403, "delegation_not_allowed")
            )
        }
        other => panic!("{other:?}"),
    }
}

/// `POST …/spawn` with `depends_on` is queued work like any spawn: the
/// worker that runs it creates the child waiting, the operation finishes
/// without starting it, and integrating its prerequisite (queued too)
/// starts it from the parent's branch.
#[test]
fn a_spawn_that_waits_goes_through_the_queue_and_starts_after_its_prerequisite() {
    let f = Fixture::new();
    let mut config = f.config();
    config.by_path = Some("/bin/true".into());
    config.allow_delegation = true;
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
    let spawn = |name: &str, prompt: &str, depends_on: &[&str]| SpawnRequest {
        prompt: prompt.into(),
        name: Some(name.into()),
        policy: PolicySpec::allow_all(),
        depends_on: depends_on.iter().map(|s| (*s).to_owned()).collect(),
        after: branchyard::After::Integrated,
        ..SpawnRequest::default()
    };
    let lib = repo
        .spawn("root", &spawn("lib", "WRITE lib.txt=1", &[]), &new_key())
        .unwrap();
    assert_eq!(
        await_operation(&client, &lib.id).state,
        OperationState::Succeeded
    );
    let app = repo
        .spawn(
            "root",
            &spawn("app", "WRITE app.txt=1", &["lib"]),
            &new_key(),
        )
        .unwrap();
    assert_eq!(app.kind, OperationKind::Spawn);
    let app = await_operation(&client, &app.id);
    assert_eq!(app.state, OperationState::Succeeded, "{app:?}");
    let inspection = app.result.unwrap().inspection.unwrap();
    assert_eq!(inspection.status, BranchStatus::Waiting);
    assert_eq!(inspection.depends_on[0].prerequisite, "lib");
    assert_eq!(repo.graph("root").unwrap().dependencies.len(), 1);
    assert_eq!(repo.branch("app").unwrap().turns, 0);

    let integrated = await_operation(&client, &repo.integrate("lib", &new_key()).unwrap().id);
    assert_eq!(
        integrated.state,
        OperationState::Succeeded,
        "{integrated:?}"
    );
    wait::until("app to start and finish", || {
        repo.branch("app").unwrap().status == BranchStatus::Ready
    });
    let app = repo.branch("app").unwrap();
    assert_eq!(app.turns, 1);
    assert!(app.worktree.join("lib.txt").is_file(), "built on lib");
}

/// A person runs a branch's check through the server, as `by check` does:
/// an operation whose result is the report; a check that fails is a
/// succeeded operation whose report says `failed`; nothing is merged; it
/// holds no branch lock and runs no turn, so the branch takes another turn
/// and a wait settles while it runs; and its idempotency key finds it
/// again.
#[test]
fn a_branchs_check_runs_over_http_as_an_operation() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();
    let repo = client.repo("app");
    let go = f.dir.join("go");
    let wait_for_go = format!("while [ ! -f \"{}\" ]; do sleep 0.05; done", go.display());
    let with_check = |name: &str, prompt: &str, check: &[&str]| TaskRequest {
        check: Some(check.iter().map(|s| (*s).to_owned()).collect()),
        ..task(prompt, name)
    };
    for request in [
        with_check("good", "WRITE ok.txt=1", &["test", "-f", "ok.txt"]),
        with_check(
            "bad",
            "WRITE other.txt=1",
            &["sh", "-c", "echo tests failed >&2; exit 1"],
        ),
        with_check("held", "WRITE held.txt=1", &["sh", "-c", &wait_for_go]),
        task("WRITE plain.txt=1", "plain"),
    ] {
        let op = run(&client, &request);
        assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    }
    let main = common::git(&f.root, &["rev-parse", "main"]);

    // A check that passes.
    let key = new_key();
    let op = repo.check("good", &key).unwrap();
    assert_eq!(
        (op.kind, op.branches.as_slice()),
        (OperationKind::Check, &["good".to_owned()][..])
    );
    let done = await_operation(&client, &op.id);
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let result = done.result.unwrap();
    assert_eq!(result.branches[0].name, "good");
    let report = result.check_report.unwrap();
    assert_eq!(report.branch, "good");
    assert_eq!(
        report.check,
        Some(vec!["test".into(), "-f".into(), "ok.txt".into()])
    );
    assert_eq!(report.outcome, CheckVerdict::Passed, "{report:?}");
    assert!(report.passed());
    assert!(report.checked.is_some(), "the check ran on a commit");
    assert_eq!(report.target, None, "a root is checked alone");

    // The key finds it again; another request under it is refused.
    let replay = repo.check("good", &key).unwrap();
    assert_eq!(replay.id, op.id);
    assert_eq!(
        repo.check("bad", &key).unwrap_err().code(),
        Some("idempotency_key_reused")
    );

    // One that fails: the operation succeeded, and the report says what
    // the local command prints.
    let failed = await_operation(&client, &repo.check("bad", &new_key()).unwrap().id);
    assert_eq!(failed.state, OperationState::Succeeded, "{failed:?}");
    let report = failed.result.unwrap().check_report.unwrap();
    assert_eq!(report.outcome, CheckVerdict::Failed, "{report:?}");
    assert!(!report.passed());
    assert!(report.output_tail.contains("tests failed"), "{report:?}");
    let yard = Yard::open(&f.root).unwrap();
    let local = yard
        .branch("bad")
        .unwrap()
        .delegate(TaskOptions::default())
        .unwrap()
        .check("bad")
        .unwrap();
    assert_eq!(
        (&local.check, local.outcome, &local.output_tail),
        (&report.check, report.outcome, &report.output_tail),
        "the report the SDK gives in the server's repository"
    );

    // A branch with no check: nothing ran, and integrating it runs none.
    let plain = await_operation(&client, &repo.check("plain", &new_key()).unwrap().id);
    let report = plain.result.unwrap().check_report.unwrap();
    assert_eq!(
        (&report.check, report.outcome),
        (&None, CheckVerdict::NotRun)
    );
    assert!(report.passed());

    // Nothing was merged.
    assert_eq!(common::git(&f.root, &["rev-parse", "main"]), main);
    assert_eq!(repo.branch("good").unwrap().status, BranchStatus::Ready);

    // It holds no branch lock and runs no turn.
    let held = repo.check("held", &new_key()).unwrap();
    wait::until("the held check to run", || {
        client.operation(&held.id).unwrap().state == OperationState::Running
    });
    let sent = repo
        .send(
            "held",
            &SendRequest {
                prompt: "WHOAMI".into(),
                policy: PolicySpec::allow_all(),
                ..SendRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    assert_eq!(
        await_operation(&client, &sent.id).state,
        OperationState::Succeeded
    );
    let waited = repo
        .wait_for(&["held"], false, Some(Duration::from_secs(10)))
        .unwrap();
    assert!(!waited.timed_out, "a check is not a turn: {waited:?}");
    let listed = repo.operations(Some("held")).unwrap();
    assert_eq!(
        (listed.len(), listed[0].kind, listed[0].state),
        (1, OperationKind::Check, OperationState::Running)
    );
    std::fs::write(&go, "").unwrap();
    let released = await_operation(&client, &held.id);
    assert_eq!(released.state, OperationState::Succeeded, "{released:?}");
    let report = released.result.unwrap().check_report.unwrap();
    assert_eq!(report.outcome, CheckVerdict::Passed, "{report:?}");

    // Refusals: no such branch, and a body that is not the shape.
    assert_eq!(
        repo.check("nope", &new_key()).unwrap_err().code(),
        Some("unknown_branch")
    );
    let (status, _, body) = raw(
        server.addr,
        &post(
            "/v1/repos/app/branches/good/check",
            Some(TOKEN),
            "",
            r#"{"target": "main"}"#,
        ),
    );
    assert_eq!(status, 400, "{body}");
    assert_eq!(json(&body)["error"]["code"], "invalid_request");
}

/// A result contract goes through the server like the rest of delegation:
/// `POST …/spawn` carries it (and refuses a schema outside the subset at
/// once), `POST …/report` carries the result and is checked against it, and
/// the child's inspection reads it back.
#[test]
fn a_result_contract_is_set_at_spawn_and_reported_against_through_the_server() {
    let f = Fixture::new();
    let mut config = f.config();
    config.by_path = Some("/bin/true".into());
    config.allow_delegation = true;
    let server = Server::start(config);
    let client = server.client();
    let repo = client.repo("app");
    let root = run(
        &client,
        &TaskRequest {
            delegation: Some(Envelope::depth(2)),
            ..task("say hi", "root")
        },
    );
    assert_eq!(root.state, OperationState::Succeeded, "{root:?}");
    let spawn = |name: &str, result: ResultContract| SpawnRequest {
        prompt: "say hi".into(),
        name: Some(name.into()),
        policy: PolicySpec::allow_all(),
        result: Some(result),
        ..SpawnRequest::default()
    };

    // A schema outside the subset is refused when it is asked for.
    let outside = ResultContract {
        schema: json!({"type": "string", "pattern": "^a"}),
        required: false,
    };
    let error = repo
        .spawn("root", &spawn("bad", outside), &new_key())
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("result.schema uses `pattern`, which the contract does not support"),
        "{error}"
    );
    assert!(repo.branch("bad").is_err(), "no child was made");

    let wanted = ResultContract::new(
        json!({"type": "object", "required": ["ok"], "properties": {"ok": {"type": "boolean"}}}),
        true,
    )
    .unwrap();
    let op = repo
        .spawn("root", &spawn("kid", wanted.clone()), &new_key())
        .unwrap();
    let op = await_operation(&client, &op.id);
    assert_eq!(op.state, OperationState::Succeeded, "{op:?}");
    let inspection = op.result.unwrap().inspection.unwrap();
    assert_eq!(inspection.result_contract, Some(wanted.clone()));
    assert_eq!(inspection.result, None);

    // The child ended its turn without the result it owes: noted to root.
    wait::until("the missing result to be noted in root's log", || {
        repo.events("root", 0).unwrap().events.iter().any(|e| {
            matches!(&e.activity, branchyard::Activity::Warning(w)
                if w.starts_with("kid ended its turn without the result its contract requires"))
        })
    });

    // A result is checked against the contract; a report without one is refused.
    let refused = |result: Option<Value>| match repo.report_result("kid", "done", result) {
        Err(error) => (error.code().map(str::to_owned), error.to_string()),
        Ok(message) => panic!("accepted: {message:?}"),
    };
    let (code, why) = refused(Some(json!({"ok": "yes"})));
    assert_eq!(code.as_deref(), Some("denied"));
    assert!(
        why.contains("result.ok: expected boolean, got string"),
        "{why}"
    );
    let (_, why) = refused(None);
    assert!(why.contains("requires a result"), "{why}");
    assert!(repo.inbox("root").unwrap().messages.is_empty());

    let good = json!({"ok": true});
    let message = repo
        .report_result("kid", "done", Some(good.clone()))
        .unwrap();
    assert_eq!(message.kind, branchyard::MessageKind::Report);
    assert_eq!(repo.inspect("kid").unwrap().result, Some(good));
    // The same value the SDK gives in the server's repository.
    let yard = Yard::open(&f.root).unwrap();
    let local = yard
        .branch("root")
        .unwrap()
        .delegate(TaskOptions::default())
        .unwrap();
    assert_eq!(repo.inspect("kid").unwrap(), local.inspect("kid").unwrap());
}
