//! The server over real HTTP on 127.0.0.1, with the fake ACP agent as the
//! `gemini-cli` harness.

mod common;

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::time::Duration;

use branchyard::BranchStatus;
use branchyard_client::api::{
    BudgetSpec, ForkRequest, MergeRequest, OperationKind, OperationState, PolicySpec, SendRequest,
};
use branchyard_client::{new_key, Client};
use common::{eventually, get, post, raw, raw_bytes, run, task, wait, Fixture, Server, TOKEN};
use serde_json::Value;

fn json(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body:?}"))
}

#[test]
fn requests_need_a_valid_token() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let (status, head, body) = raw(server.addr, &get("/v1/repos", None));
    assert_eq!(status, 401);
    assert!(
        head.to_ascii_lowercase()
            .contains("www-authenticate: bearer"),
        "{head}"
    );
    assert_eq!(json(&body)["error"]["code"], "unauthorized");

    for wrong in ["wrong-token-0123456789", "test-token-012345678", ""] {
        let (status, _, body) = raw(server.addr, &get("/v1/repos", Some(wrong)));
        assert_eq!(status, 401, "{wrong:?}");
        assert!(!body.contains(TOKEN));
    }
    let basic = "GET /v1/repos HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
                 Authorization: Basic dGVzdDp0ZXN0\r\n\r\n";
    assert_eq!(raw(server.addr, basic).0, 401);
    // Even an unknown route under /v1 is refused first.
    assert_eq!(raw(server.addr, &get("/v1/nope", None)).0, 401);

    let (status, head, body) = raw(server.addr, &get("/v1/repos", Some(TOKEN)));
    assert_eq!(status, 200, "{body}");
    assert!(
        head.to_ascii_lowercase().contains("x-request-id: req_"),
        "{head}"
    );
    assert_eq!(json(&body)["repos"][0]["name"], "app");
    assert_eq!(raw(server.addr, &get("/healthz", None)).0, 200);

    // The typed client reports the same refusal.
    let wrong = Client::new(&server.url(), "wrong-token-0123456789").unwrap();
    let error = wrong.repos().unwrap_err();
    assert_eq!(error.code(), Some("unauthorized"));
    assert_eq!(error.to_string(), "a valid bearer token is required");
}

#[test]
fn a_repeated_idempotency_key_runs_once() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let repo = server.client().repo("app");
    let request = task("WRITE once.txt=1", "once");

    // Racing retries all get the same operation.
    let barrier = Arc::new(Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let (repo, request, barrier) = (repo.clone(), request.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                repo.submit_task(&request, "key-1").unwrap().id
            })
        })
        .collect();
    let ids: HashSet<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(ids.len(), 1, "{ids:?}");
    let id = ids.into_iter().next().unwrap();
    let done = wait(repo.client(), &id);
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");

    // After it finished, the key still returns it, with its result.
    let body = serde_json::to_string(&request).unwrap();
    let (status, head, text) = raw(
        server.addr,
        &post(
            "/v1/repos/app/tasks",
            Some(TOKEN),
            "Idempotency-Key: key-1\r\n",
            &body,
        ),
    );
    assert_eq!(status, 200, "{text}");
    assert!(head
        .to_ascii_lowercase()
        .contains("idempotent-replayed: true"));
    assert_eq!(json(&text)["id"], id.as_str());
    assert_eq!(json(&text)["result"]["branches"][0]["name"], "once");
    assert_eq!(repo.branches().unwrap().len(), 1, "ran once");

    // The same key for another request is refused, not run.
    let other = repo.submit_task(&task("WRITE twice.txt=1", "twice"), "key-1");
    assert_eq!(other.unwrap_err().code(), Some("idempotency_key_reused"));
    // Keys are per caller and per request; a new key runs anew.
    let again = repo
        .submit_task(&task("WRITE once.txt=1", "again"), "key-2")
        .unwrap();
    assert_eq!(
        wait(repo.client(), &again.id).state,
        OperationState::Succeeded
    );
    assert_eq!(repo.branches().unwrap().len(), 2);
}

#[test]
fn a_task_runs_through_merge_over_http() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();
    let repo = client.repo("app");

    let (status, head, body) = raw(
        server.addr,
        &post(
            "/v1/repos/app/tasks",
            Some(TOKEN),
            "",
            r#"{"prompt": "WRITE hello.txt=hi", "harness": "gemini-cli", "name": "hello",
                "policy": {"mode": "allow"}, "budget": {"max_turns": 5}}"#,
        ),
    );
    assert_eq!(status, 202, "{body}");
    let accepted = json(&body);
    let id = accepted["id"].as_str().unwrap().to_owned();
    assert!(
        head.contains(&format!("location: /v1/operations/{id}")),
        "{head}"
    );
    assert_eq!(accepted["kind"], "task");
    assert_eq!(accepted["branches"], serde_json::json!(["hello"]));
    let done = wait(&client, &id);
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let info = &done.result.as_ref().unwrap().branches[0];
    assert_eq!(info.status, BranchStatus::Ready);
    assert_eq!(info.profile, "gemini-cli-acp");

    let branches = repo.branches().unwrap();
    assert_eq!(branches.len(), 1);
    assert_eq!(repo.branch("hello").unwrap(), *info);
    assert!(repo.diff("hello").unwrap().contains("+hi"));
    let events = repo.events("hello", 0).unwrap();
    assert!(events.events.len() > 3);
    let later = repo.events("hello", 2).unwrap();
    assert_eq!(later.events[..], events.events[2..]);
    assert_eq!(later.cursor, events.cursor);
    assert!(repo
        .events("hello", events.cursor)
        .unwrap()
        .events
        .is_empty());

    // Continue the session, then fork it.
    let sent = repo
        .send(
            "hello",
            &SendRequest {
                prompt: "WHOAMI".into(),
                ..SendRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    let sent = wait(&client, &sent.id);
    assert_eq!(sent.kind, OperationKind::Send);
    assert_eq!(sent.result.unwrap().branches[0].turns, 2);
    assert!(common_text(&repo, "hello").contains("resumed=true"));
    let forked = repo
        .fork(
            "hello",
            &ForkRequest {
                prompt: "WRITE fork.txt=f".into(),
                name: Some("hello-fork".into()),
                fresh_session: true,
                policy: PolicySpec::allow_all(),
                ..ForkRequest::default()
            },
            &new_key(),
        )
        .unwrap();
    assert_eq!(forked.branches, ["hello-fork"]);
    let forked = wait(&client, &forked.id);
    let fork_info = &forked.result.unwrap().branches[0];
    assert_eq!(fork_info.parent.as_deref(), Some("hello"));
    assert_eq!(fork_info.status, BranchStatus::Ready);

    // Merge into the checked-out branch by default.
    let merge = repo
        .merge("hello", &MergeRequest::default(), &new_key())
        .unwrap();
    let merged = wait(&client, &merge.id);
    assert_eq!(merged.state, OperationState::Succeeded, "{merged:?}");
    let result = merged.result.unwrap();
    let info = result.merged.unwrap();
    assert_eq!(info.target, "main");
    assert_eq!(
        common::git(&f.root, &["rev-parse", "main"]).trim(),
        info.commit
    );
    assert!(matches!(
        result.branches[0].status,
        BranchStatus::Merged { .. }
    ));
    // Merging again fails with the SDK's message and a stable code.
    let again = repo
        .merge("hello", &MergeRequest::default(), &new_key())
        .unwrap();
    let again = wait(&client, &again.id);
    assert_eq!(again.state, OperationState::Failed);
    let error = again.error.unwrap();
    assert_eq!(error.code, "already_merged");
    assert_eq!(error.message, "the candidate is already contained in main");

    repo.remove("hello-fork").unwrap();
    let missing = repo.branch("hello-fork").unwrap_err();
    assert_eq!(missing.code(), Some("unknown_branch"));
    assert_eq!(missing.to_string(), "no branch named hello-fork");
    assert_eq!(
        repo.remove("hello-fork").unwrap_err().code(),
        Some("unknown_branch")
    );
    let unknown = client.repo("nope").branches().unwrap_err();
    assert_eq!(unknown.code(), Some("unknown_repo"));
    assert_eq!(
        client.operation("op_nope").unwrap_err().code(),
        Some("unknown_operation")
    );
    let harnesses = client.harnesses().unwrap();
    assert!(harnesses.iter().any(|h| h.harness == "gemini-cli"));
}

fn common_text(repo: &branchyard_client::Repo, branch: &str) -> String {
    repo.events(branch, 0)
        .unwrap()
        .events
        .iter()
        .filter_map(|e| match &e.activity {
            branchyard::Activity::Harness(branchyard::Event::MessageDelta { text, .. }) => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn the_event_stream_resumes_by_cursor() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();
    let repo = client.repo("app");

    // Live: a stream opened at the head (0) before the task sees its
    // activity as it happens.
    let (tx, rx) = std::sync::mpsc::channel();
    let live = repo.stream(Some(0));
    std::thread::spawn(move || {
        for entry in live {
            if tx.send(entry).is_err() {
                return;
            }
        }
    });
    let first = run(&client, &task("WRITE a.txt=2", "first"));
    let end = first.end_cursor.unwrap();
    assert!(end > 5, "{first:?}");
    let mut seen = Vec::new();
    while seen.len() < end as usize {
        let entry = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        seen.push(entry);
    }
    let seqs: Vec<u64> = seen.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=end).collect::<Vec<_>>(), "gap-free and in order");
    assert!(seen.iter().all(|e| e.branch == "first"));
    let recorded = repo.events("first", 0).unwrap().events;
    assert_eq!(recorded.len() as u64, end, "one entry per recorded event");
    assert_eq!(seen[0].activity, recorded[0].activity);

    // Resume in the middle, by query and by Last-Event-ID.
    let second = run(&client, &task("WRITE b.txt=3", "second"));
    let middle = end - 2;
    let resumed: Vec<_> = repo
        .stream(Some(middle))
        .take(4)
        .map(|e| e.unwrap())
        .collect();
    assert_eq!(
        resumed.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [middle + 1, middle + 2, middle + 3, middle + 4]
    );
    assert_eq!(resumed[0], seen[middle as usize]);
    assert_eq!(resumed[2].branch, "second");

    let mut stream = TcpStream::connect(server.addr).unwrap();
    write!(
        stream,
        "GET /v1/repos/app/events/stream?cursor=0 HTTP/1.1\r\nHost: x\r\n\
         Authorization: Bearer {TOKEN}\r\nLast-Event-ID: {end}\r\n\r\n"
    )
    .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut ids = Vec::new();
    let mut line = String::new();
    while ids.len() < 2 {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if let Some(id) = line.trim().strip_prefix("id: ") {
            ids.push(id.parse::<u64>().unwrap());
        }
    }
    assert_eq!(
        ids,
        [end, end + 1],
        "an open event at the cursor, then the next entry"
    );
    drop(reader);

    // A cursor past the end is refused rather than waited on.
    let head = second.end_cursor.unwrap();
    let error = repo.stream(Some(head + 5)).next().unwrap().unwrap_err();
    assert_eq!(error.code(), Some("cursor_out_of_range"));
}

#[test]
fn a_running_turn_is_cancelled_over_http() {
    let f = Fixture::new();
    let server = Server::start(f.config());
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
    // The branch lock is the operation's; a cancel does not need it.
    assert_eq!(repo.cancel("held").unwrap(), ["held"]);
    let done = wait(&client, &op.id);
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    assert_eq!(
        done.result.unwrap().branches[0].status,
        BranchStatus::Interrupted
    );
    let events = repo.events("held", 0).unwrap().events;
    assert!(events.iter().any(|e| e.activity
        == branchyard::Activity::Warning("cancelled by tester through the server".into())));
    assert!(repo.cancel("held").unwrap().is_empty(), "nothing runs now");

    let missing = repo.cancel("nope").unwrap_err();
    assert_eq!(missing.code(), Some("unknown_branch"));
    let (status, _, body) = raw(
        server.addr,
        &post(
            "/v1/repos/app/branches/held/cancel",
            Some(TOKEN),
            "",
            r#"{"subtree": false}"#,
        ),
    );
    assert_eq!(status, 400, "{body}");
    assert_eq!(json(&body)["error"]["code"], "invalid_request");
}

#[test]
fn operations_survive_a_restart() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();
    let repo = client.repo("app");
    let done = run(&client, &task("WRITE kept.txt=1", "kept"));
    let key = new_key();
    let mut hang = task("HANG", "hang");
    hang.budget = BudgetSpec {
        max_seconds: Some(2.0),
        ..BudgetSpec::default()
    };
    let hanging = repo.submit_task(&hang, &key).unwrap();
    eventually("the hanging turn to start", || {
        client.operation(&hanging.id).unwrap().state == OperationState::Running
    });
    // A graceful shutdown lets running turns finish.
    let stopped = server.stop();
    assert_eq!(stopped.interrupted, 0);

    let mut config = f.config();
    config.shutdown_grace = Duration::ZERO;
    let server = Server::start(config.clone());
    let client = server.client();
    let repo = client.repo("app");
    assert_eq!(client.operation(&done.id).unwrap(), done);
    let finished = client.operation(&hanging.id).unwrap();
    assert_eq!(finished.state, OperationState::Succeeded, "{finished:?}");
    assert!(matches!(
        finished.result.unwrap().branches[0].status,
        BranchStatus::BudgetExceeded { .. }
    ));
    // The key still maps to the same operation after the restart.
    assert_eq!(repo.submit_task(&hang, &key).unwrap().id, hanging.id);
    // The feed is durable too.
    let head = finished.end_cursor.unwrap();
    let replayed: Vec<u64> = repo
        .stream(Some(0))
        .take(head as usize)
        .map(|e| e.unwrap().seq)
        .collect();
    assert_eq!(replayed, (1..=head).collect::<Vec<_>>());

    // Without grace, a turn still running at shutdown is recorded as
    // interrupted, and stays so across the next restart.
    let mut long = task("HANG", "long");
    long.budget.max_seconds = Some(2.0);
    let long = repo.submit_task(&long, &new_key()).unwrap();
    eventually("the long turn to start", || {
        client.operation(&long.id).unwrap().state == OperationState::Running
    });
    let stopped = server.stop();
    assert_eq!(stopped.interrupted, 1);
    let server = Server::start(config);
    let client = server.client();
    let op = client.operation(&long.id).unwrap();
    assert_eq!(op.state, OperationState::Interrupted);
    assert_eq!(op.error.unwrap().code, "interrupted");
    // In this test the turn's thread outlives the stopped server (a real
    // process exit would end it); let it finish before cleaning up.
    eventually("the orphaned turn to end", || {
        client
            .repo("app")
            .branch("long")
            .is_ok_and(|b| b.status != BranchStatus::Running)
    });
}

#[test]
fn a_client_disconnect_does_not_stop_a_turn() {
    let f = Fixture::new();
    let server = Server::start(f.config());
    let client = server.client();
    let repo = client.repo("app");

    // Submit, read the acceptance, and hang up while the turn runs; an
    // event stream is dropped mid-turn too.
    let body = r#"{"prompt": "HANG", "harness": "gemini-cli", "name": "left",
                   "budget": {"max_seconds": 3}}"#;
    let mut stream = TcpStream::connect(server.addr).unwrap();
    stream
        .write_all(post("/v1/repos/app/tasks", Some(TOKEN), "", body).as_bytes())
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    assert!(status.contains("202"), "{status}");
    drop(reader);
    let watching = repo.stream(Some(0)).next().unwrap().unwrap();
    assert_eq!(watching.branch, "left");
    eventually("the branch to start", || {
        repo.branch("left")
            .is_ok_and(|b| b.status == BranchStatus::Running)
    });
    // While it runs, the branch is locked against other changes.
    let busy = repo
        .send(
            "left",
            &SendRequest {
                prompt: "x".into(),
                ..SendRequest::default()
            },
            &new_key(),
        )
        .unwrap_err();
    assert_eq!(busy.code(), Some("branch_busy"));
    assert_eq!(repo.remove("left").unwrap_err().code(), Some("branch_busy"));
    eventually("the turn to reach its own limit", || {
        matches!(
            repo.branch("left").unwrap().status,
            BranchStatus::BudgetExceeded { .. }
        )
    });

    // A client that hangs up before any response, then retries with the
    // same key, gets one run.
    let body = serde_json::to_string(&task("WRITE r.txt=1", "retried")).unwrap();
    let request = post(
        "/v1/repos/app/tasks",
        Some(TOKEN),
        "Idempotency-Key: retry-1\r\n",
        &body,
    );
    let mut abandoned = TcpStream::connect(server.addr).unwrap();
    abandoned.write_all(request.as_bytes()).unwrap();
    drop(abandoned);
    std::thread::sleep(Duration::from_millis(100));
    let op = repo
        .submit_task(&task("WRITE r.txt=1", "retried"), "retry-1")
        .unwrap();
    assert_eq!(wait(&client, &op.id).state, OperationState::Succeeded);
    let named: Vec<_> = repo
        .branches()
        .unwrap()
        .into_iter()
        .filter(|b| b.name.starts_with("retried"))
        .collect();
    assert_eq!(named.len(), 1, "{named:?}");
}

#[test]
fn request_bodies_are_bounded_and_checked() {
    let f = Fixture::new();
    let mut config = f.config();
    config.max_body_bytes = 4096;
    let server = Server::start(config);
    let big = format!(r#"{{"prompt": "{}"}}"#, "x".repeat(5000));
    let (status, _, body) = raw(
        server.addr,
        &post("/v1/repos/app/tasks", Some(TOKEN), "", &big),
    );
    assert_eq!(status, 413, "{body}");
    assert_eq!(json(&body)["error"]["code"], "body_too_large");
    assert_eq!(json(&body)["error"]["detail"]["limit"], 4096);

    // Chunked, without a declared length, is bounded the same way.
    let chunk = "y".repeat(6000);
    let chunked = format!(
        "POST /v1/repos/app/tasks HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Authorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\n\
         Transfer-Encoding: chunked\r\n\r\n{:x}\r\n{chunk}\r\n0\r\n\r\n",
        chunk.len()
    );
    assert_eq!(raw_bytes(server.addr, chunked.as_bytes()).0, 413);

    let plain = "POST /v1/repos/app/tasks HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
                 Authorization: Bearer test-token-0123456789\r\nContent-Length: 2\r\n\r\n{}";
    let (status, _, body) = raw(server.addr, plain);
    assert_eq!(
        (status, json(&body)["error"]["code"].clone()),
        (415, "unsupported_media_type".into())
    );
    for (bad, needle) in [
        ("{", "invalid request body"),
        (r#"{"prompt": "x", "harnes": "y"}"#, "harnes"),
        (r#"{"prompt": "  "}"#, "prompt is empty"),
        (r#"{"prompt": "x", "budget": {"max_usd": -1}}"#, "max_usd"),
        (
            r#"{"prompt": "x", "command": ["/bin/sh"]}"#,
            "does not accept",
        ),
    ] {
        let (status, _, body) = raw(
            server.addr,
            &post("/v1/repos/app/tasks", Some(TOKEN), "", bad),
        );
        assert!(status == 400 || status == 403, "{bad}: {status} {body}");
        assert!(body.contains(needle), "{bad}: {body}");
    }
    let (status, _, body) = raw(server.addr, &get("/v1/repos/app/tasks", Some(TOKEN)));
    assert_eq!(
        (status, json(&body)["error"]["code"].clone()),
        (405, "method_not_allowed".into())
    );
    let (status, _, _) = raw(
        server.addr,
        &get("/v1/repos/app/branches/x/events?cursor=-1", Some(TOKEN)),
    );
    assert_eq!(status, 400);
}

#[test]
fn plain_http_binds_only_to_loopback() {
    let f = Fixture::new();
    let mut config = f.config();
    config.listen = "0.0.0.0:0".parse().unwrap();
    let error = Server::try_start(config.clone()).err().unwrap();
    assert!(
        error.contains("refusing to serve plain HTTP on 0.0.0.0:0"),
        "{error}"
    );
    assert!(error.contains("--insecure-bind"), "{error}");
    assert!(!f.data.exists(), "nothing was created");

    let mut insecure = config.clone();
    insecure.insecure_bind = true;
    let warning = insecure.validate().unwrap().unwrap();
    assert!(warning.contains("unencrypted"), "{warning}");
    let server = Server::start(insecure);
    assert!(server.addr.ip().is_unspecified());
    drop(server);

    // With TLS it serves HTTPS, and a client trusting the certificate
    // connects.
    let fixtures = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures"));
    let mut tls = f.config();
    tls.tls = Some(branchyard_server::config::TlsFiles {
        cert: fixtures.join("localhost-test-only.crt"),
        key: fixtures.join("localhost-test-only.key"),
    });
    let server = Server::start(tls);
    let url = format!("https://127.0.0.1:{}", server.addr.port());
    let client = Client::new(&url, TOKEN)
        .unwrap()
        .with_ca_file(fixtures.join("localhost-test-only.crt"))
        .unwrap();
    assert_eq!(client.repos().unwrap()[0].name, "app");
    // Without trusting it, the handshake fails; plain HTTP gets nothing.
    let untrusting = Client::new(&url, TOKEN).unwrap();
    assert!(matches!(
        untrusting.repos().unwrap_err(),
        branchyard_client::Error::Transport { .. }
    ));
    assert!(
        Client::new(&format!("http://127.0.0.1:{}", server.addr.port()), TOKEN)
            .unwrap()
            .repos()
            .is_err()
    );
}

#[test]
fn secrets_are_named_by_the_request_and_resolved_by_the_server() {
    let f = Fixture::new();
    std::env::set_var("BY_TEST_SERVER_GEMINI", "server-side-gemini-secret");
    let mut config = f.config();
    config.secrets.insert(
        "GEMINI_API_KEY".into(),
        branchyard::SecretSource::parse("GEMINI_API_KEY=BY_TEST_SERVER_GEMINI").unwrap(),
    );
    let server = Server::start(config);
    let client = server.client();
    let provision = |secret: &str| branchyard::Provisioning {
        secrets: vec![branchyard::SecretSource::parse(secret).unwrap()],
        model: Some("gemini-server-model".into()),
        ..branchyard::Provisioning::default()
    };
    let request = branchyard_client::api::TaskRequest {
        isolated: true,
        provision: Some(provision("GEMINI_API_KEY")),
        ..task(
            "SH test ${#GEMINI_API_KEY} -eq 25 && echo key-from-server\n\
             SH cat \"$HOME/.gemini/settings.json\"",
            "served",
        )
    };
    let done = run(&client, &request);
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let said = common_text(&client.repo("app"), "served");
    assert!(said.contains("key-from-server"), "{said}");
    assert!(said.contains("gemini-server-model"), "{said}");
    let events = client.repo("app").events("served", 0).unwrap();
    assert!(!serde_json::to_string(&events.events)
        .unwrap()
        .contains("server-side-gemini-secret"));

    // A request never picks a source, names only secrets the server
    // defines, and runs no MCP server unless client commands are allowed.
    let refusals = [
        (provision("GEMINI_API_KEY=HOME"), 400, "invalid_request"),
        (provision("OPENAI_API_KEY"), 403, "secret_not_allowed"),
        (
            branchyard::Provisioning {
                mcp_servers: vec![branchyard::McpServerSpec::parse("x=/bin/sh").unwrap()],
                ..branchyard::Provisioning::default()
            },
            403,
            "command_not_allowed",
        ),
    ];
    for (provision, status, code) in refusals {
        let request = branchyard_client::api::TaskRequest {
            isolated: true,
            provision: Some(provision),
            ..task("x", "refused")
        };
        let error = client
            .repo("app")
            .submit_task(&request, &new_key())
            .unwrap_err();
        assert_eq!(error.code(), Some(code), "{error}");
        assert!(
            matches!(error, branchyard_client::Error::Api { status: s, .. } if s == status),
            "{error:?}"
        );
    }
}

#[test]
fn a_rigs_seats_are_checked_when_its_task_is_submitted() {
    let f = Fixture::new();
    let seat = branchyard::Seat {
        harness: "gemini-cli".into(),
        budget: branchyard::ChildBudget::default(),
        check: None,
        deny: Vec::new(),
        isolated: true,
        provision: Some(branchyard::Provisioning {
            secrets: vec![branchyard::SecretSource::parse("OPENAI_API_KEY").unwrap()],
            ..branchyard::Provisioning::default()
        }),
        delegates_to: Vec::new(),
        instances: 1,
    };
    let seats = branchyard::Seats {
        rig: "team".into(),
        seat: "lead".into(),
        delegates_to: vec!["worker".into()],
        table: [("worker".to_owned(), seat)].into_iter().collect(),
    };
    let submit = |client: &Client, seats: branchyard::Seats, delegation: bool| {
        let request = branchyard_client::api::TaskRequest {
            delegation: delegation.then(|| seats.envelope()),
            seats: Some(seats),
            ..task("x", "rig")
        };
        client
            .repo("app")
            .submit_task(&request, &new_key())
            .unwrap_err()
    };
    // Seats are delegation, so the operator must allow it.
    let plain = Server::start(f.config());
    let error = submit(&plain.client(), seats.clone(), true);
    assert_eq!(error.code(), Some("delegation_not_allowed"), "{error}");
    drop(plain);

    let mut config = f.config();
    config.allow_delegation = true;
    let server = Server::start(config);
    let client = server.client();
    let error = submit(&client, seats.clone(), false);
    assert_eq!(error.code(), Some("invalid_request"), "{error}");
    assert!(
        error.to_string().contains("need a delegation envelope"),
        "{error}"
    );
    let mut loose = seats.clone();
    loose.delegates_to.clear();
    let error = submit(&client, loose, true);
    assert_eq!(error.code(), Some("invalid_request"), "{error}");
    assert!(
        error.to_string().contains("worker is not below seat lead"),
        "{error}"
    );
    // A seat's secrets are the server's to define, like a task's.
    let error = submit(&client, seats, true);
    assert_eq!(error.code(), Some("secret_not_allowed"), "{error}");
}
