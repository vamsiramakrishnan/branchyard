//! A server with `sync`, over real HTTP with the fake ACP agent: a task's
//! branch pushed to a `file://` remote after it runs; an operation refused
//! while another runner holds the task's lease; work another machine
//! pushed pulled before the next turn, which runs on top of it; and the
//! sync series in `/metrics`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use branchyard_client::api::{OperationState, SendRequest};
use branchyard_client::new_key;
use branchyard_server::config::{MetricsConfig, SyncSettings};
use branchyard_sync::engine::{Options, Remote, TaskState};
use branchyard_sync::source::BranchSource;
use branchyard_sync::store::file::FileStore;
use branchyard_sync::SyncConfig;
use common::{eventually, get, git, raw, run, task, wait, Fixture, Server, TOKEN};

fn remote(bucket: &std::path::Path, device: &str) -> Remote {
    let mut options = Options::default();
    options.settings.device = device.into();
    Remote::open(Arc::new(FileStore::open(bucket).unwrap()), options).unwrap()
}

#[test]
fn a_server_pulls_runs_under_the_lease_and_pushes() {
    let f = Fixture::new();
    let bucket = f.dir.join("bucket");
    let mut config = f.config();
    config.metrics = Some(MetricsConfig {
        listen: None,
        token_sha256: None,
    });
    config.sync = Some(SyncSettings {
        config: SyncConfig {
            remote: format!("file://{}", bucket.display()),
            interval: Some("1s".into()),
            device: Some("server".into()),
            ..SyncConfig::default()
        },
        lease: Duration::from_secs(30),
    });
    let server = Server::start(config);
    let client = server.client();
    let done = run(&client, &task("WRITE one.txt=1", "s1"));
    assert_eq!(done.state, OperationState::Succeeded, "{done:?}");

    // Pushed after it ran, keyed by the repository.
    let elsewhere = remote(&bucket, "laptop");
    eventually("the task in the remote", || {
        elsewhere
            .tasks()
            .is_ok_and(|t| t.iter().any(|t| t.task.ends_with(".s1")))
    });
    let task_id = elsewhere
        .tasks()
        .unwrap()
        .into_iter()
        .find(|t| t.task.ends_with(".s1"))
        .unwrap()
        .task;
    assert!(
        elsewhere.lease_state(&task_id, "run").unwrap().is_none(),
        "the lease was released"
    );

    // Another machine, a clone, pulls it, works on it and pushes.
    let other = f.dir.join("other");
    git(
        &f.dir,
        &["clone", "-q", &f.root.display().to_string(), "other"],
    );
    git(&other, &["config", "user.name", "Laptop"]);
    git(&other, &["config", "user.email", "laptop@localhost"]);
    let source = BranchSource::new(&other.join(".git"), "s1", "s1")
        .unwrap()
        .with_task_id(&task_id)
        .unwrap();
    let mut state = TaskState::default();
    elsewhere.pull(&source, &mut state).unwrap();
    git(&other, &["checkout", "-q", "s1"]);
    std::fs::write(other.join("laptop.txt"), "from the laptop\n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-q", "-m", "from the laptop"]);
    let laptop = git(&other, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&other, &["checkout", "-q", "--detach"]);
    let report = elsewhere.sync(&source, &mut state).unwrap();
    assert!(report.swapped, "{report:?}");

    // While the laptop holds the task's lease, the server refuses to run it.
    let held = elsewhere
        .acquire_lease(&task_id, "run", "laptop:1", Duration::from_secs(300))
        .unwrap();
    let repo = client.repo("app");
    let send = |prompt: &str| {
        let op = repo
            .send(
                "s1",
                &SendRequest {
                    prompt: prompt.into(),
                    ..SendRequest::default()
                },
                &new_key(),
            )
            .unwrap();
        wait(&client, &op.id)
    };
    let refused = send("WRITE two.txt=2");
    assert_eq!(refused.state, OperationState::Failed, "{refused:?}");
    let error = refused.error.unwrap();
    assert_eq!(error.code, "sync_lease_held", "{error:?}");
    assert!(error.message.contains("laptop:1"), "{error:?}");

    // Released, the server pulls the laptop's commit first and runs on it.
    elsewhere.release_lease(held).unwrap();
    let sent = send("WRITE two.txt=2");
    assert_eq!(sent.state, OperationState::Succeeded, "{sent:?}");
    let yard = branchyard::Yard::open(&f.root).unwrap();
    let git_branch = yard.branch("s1").unwrap().info().git_branch.clone();
    let tip = git(&f.root, &["rev-parse", &format!("refs/heads/{git_branch}")]);
    let ancestor = std::process::Command::new("git")
        .arg("-C")
        .arg(&f.root)
        .args(["merge-base", "--is-ancestor", &laptop, tip.trim()])
        .status()
        .unwrap();
    assert!(
        ancestor.success(),
        "the turn ran on top of the laptop's work"
    );

    // And the result reaches the remote.
    eventually("the second turn pushed", || {
        elsewhere
            .manifest(&task_id)
            .ok()
            .flatten()
            .is_some_and(|(m, _)| {
                m.refs.get("refs/heads/main").map(String::as_str) == Some(tip.trim())
            })
    });

    let (status, _, text) = raw(server.addr, &get("/metrics", Some(TOKEN)));
    assert_eq!(status, 200);
    let swaps = text
        .lines()
        .find_map(|l| l.strip_prefix(r#"branchyard_sync_swaps_total{repo="app"} "#))
        .map(|v| v.parse::<f64>().unwrap())
        .unwrap_or(0.0);
    assert!(swaps >= 2.0, "{text}");
    assert!(
        text.contains(r#"branchyard_sync_bytes_total{direction="up",repo="app"}"#)
            || text.contains(r#"branchyard_sync_bytes_total{repo="app",direction="up"}"#),
        "{text}"
    );
    assert!(text.contains("branchyard_sync_lag_seconds"), "{text}");
    server.stop();
}
