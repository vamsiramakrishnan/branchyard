//! A served repository's `[workspace]` runs only when the server's operator
//! allows it (`allow_workspace_scripts`), never because a request asks,
//! and a configuration naming a repository it does not serve is refused.

mod common;

use std::fs;

use branchyard::{BranchStatus, WorkspacePhase};
use branchyard_client::api::OperationState;
use branchyard_server::config::WorkspaceScripts;
use common::{post, raw, run, task, Fixture, Server, TOKEN};

const PROJECT: &str = r#"
[workspace]
copy = [".env"]
setup = "echo $BRANCHYARD_PORT > setup-ran"
teardown = "touch $BRANCHYARD_ROOT/../torn-down"
"#;

fn fixture() -> Fixture {
    let f = Fixture::new();
    fs::write(
        f.root.join(".gitignore"),
        "branchyard.toml\n.env\nsetup-ran\n",
    )
    .unwrap();
    common::git(&f.root, &["add", ".gitignore"]);
    common::git(&f.root, &["commit", "-q", "-m", "ignore"]);
    fs::write(f.root.join("branchyard.toml"), PROJECT).unwrap();
    fs::write(f.root.join(".env"), "A=1\n").unwrap();
    f
}

#[test]
fn a_repositorys_scripts_run_only_when_the_operator_allows_them() {
    let f = fixture();
    let server = Server::start(f.config());
    let client = server.client();
    let done = run(&client, &task("WRITE x.txt=1", "plain"));
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let info = &done.result.as_ref().unwrap().branches[0];
    assert_eq!(info.status, BranchStatus::Ready);
    assert!(!info.worktree.join("setup-ran").exists());
    assert!(!info.worktree.join(".env").exists());

    // A request cannot ask for it.
    let (status, _, body) = raw(
        server.addr,
        &post(
            "/v1/repos/app/tasks",
            Some(TOKEN),
            "",
            r#"{"prompt": "WRITE y.txt=1", "harness": "gemini-cli", "name": "sneaky",
                "policy": {"mode": "allow"},
                "workspace": {"setup": ["touch pwned"]}}"#,
        ),
    );
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("workspace"), "{body}");
    drop(server);

    let mut config = f.config();
    config.allow_workspace_scripts = WorkspaceScripts::Repos(["app".to_owned()].into());
    let server = Server::start(config);
    let client = server.client();
    let done = run(&client, &task("WRITE x.txt=1", "prepared"));
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");
    let info = &done.result.as_ref().unwrap().branches[0];
    assert_eq!(info.status, BranchStatus::Ready);
    let port: u16 = fs::read_to_string(info.worktree.join("setup-ran"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(info.worktree.join(".env").is_file());
    let events = client.repo("app").events("prepared", 0).unwrap();
    assert!(events.events.iter().any(|e| matches!(
        &e.activity,
        branchyard::Activity::Workspace(r) if r.phase == WorkspacePhase::Setup && r.port == Some(port)
    )));
    client.repo("app").remove("prepared").unwrap();
    assert!(f.dir.join("torn-down").exists());

    // A branch created before stays without a workspace; a local branch
    // with scripts, removed through a server that does not allow them, has
    // its teardown skipped.
    drop(server);
    let yard = branchyard::Yard::open(&f.root).unwrap();
    yard.task("WRITE z.txt=1")
        .options(branchyard::TaskOptions {
            harness: Some("gemini-cli".into()),
            command: Some(vec![common::fake_agent().display().to_string()]),
            workspace: Some(branchyard::WorkspaceSpec {
                teardown: vec!["touch $BRANCHYARD_ROOT/../local-torn".into()],
                ..Default::default()
            }),
            ..Default::default()
        })
        .name("local")
        .run()
        .unwrap();
    drop(yard);
    let server = Server::start(f.config());
    server.client().repo("app").remove("local").unwrap();
    assert!(!f.dir.join("local-torn").exists());
}

#[test]
fn allowing_a_repository_the_server_does_not_serve_is_refused() {
    let f = fixture();
    let mut config = f.config();
    config.allow_workspace_scripts = WorkspaceScripts::Repos(["elsewhere".to_owned()].into());
    let error = config.validate().unwrap_err();
    assert!(
        error.contains("allow_workspace_scripts names elsewhere"),
        "{error}"
    );
    let path = f.dir.join("server.json");
    fs::write(
        &path,
        format!(
            r#"{{"repos": {{"app": "{}"}}, "data_dir": "{}", "allow_workspace_scripts": true,
                "tokens": [{{"name": "t", "token": "{TOKEN}"}}]}}"#,
            f.root.display(),
            f.data.display()
        ),
    )
    .unwrap();
    let partial = branchyard_server::config::load_file(&path).unwrap();
    assert_eq!(partial.allow_workspace_scripts, WorkspaceScripts::All);
}
