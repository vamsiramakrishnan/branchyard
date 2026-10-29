//! Real local PTYs and fake-provider workspace interaction; no model calls.
mod common;

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use branchyard::workspace_runtime::{RuntimeAction, Terminal, TerminalOptions};
use branchyard::{
    Error, Policy, Provider, SandboxKeep, SandboxOptions, TaskOptions, WorkspaceSpec,
};
use branchyard_runtime::LocalProvider;
use branchyard_sandbox::fake::{FakeOp, FakeProvider};
use branchyard_sandbox::{Process as _, SandboxState};
use branchyard_testkit::{wait, MockHttp, Response};
use common::Fixture;

fn branch(f: &Fixture, name: &str) {
    f.yard
        .task("say hello")
        .options(TaskOptions {
            workspace: Some(WorkspaceSpec::default()),
            ..f.options()
        })
        .name(name)
        .run()
        .unwrap();
}

fn terminal(f: &Fixture, name: &str) -> Terminal {
    Terminal::start(
        f.yard.workspace_runtime(name).unwrap(),
        TerminalOptions {
            python: "/usr/bin/python3".into(),
            ..TerminalOptions::default()
        },
        Arc::new(|| Ok(())),
    )
    .unwrap()
}

fn terminal_pid(f: &Fixture, name: &str, terminal: &Terminal) -> u32 {
    let path = f
        .yard
        .branch(name)
        .unwrap()
        .info()
        .worktree
        .join("terminal-pid");
    terminal.write(b"printf '%s' $$ > terminal-pid\n").unwrap();
    wait::until("PTY shell records its process identity", || {
        std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 0)
    });
    let pid = std::fs::read_to_string(path).unwrap().parse().unwrap();
    assert!(branchyard::services::Reclaim::process(pid, true).is_some());
    pid
}

fn remote(f: &Fixture, name: &str) -> Arc<FakeProvider> {
    let provider = Arc::new(FakeProvider::live(
        Box::new(LocalProvider::new()),
        f.dir.join("provider"),
    ));
    f.yard.use_sandbox_provider(provider.clone());
    f.yard
        .task("say hello")
        .options(TaskOptions {
            provider: Some(Provider::Microsandbox(SandboxOptions {
                image: "fixture:1".into(),
                keep: SandboxKeep::Pause,
                ..SandboxOptions::default()
            })),
            policy: Policy::allow_all(),
            ..f.options()
        })
        .name(name)
        .run()
        .unwrap();
    provider
}

#[test]
fn a_workspace_helper_holds_the_existing_branch_lease() {
    let f = Fixture::new();
    branch(&f, "local");
    let session = f.yard.branch("local").unwrap().info().session.clone();
    let runtime = f.yard.workspace_runtime("local").unwrap();
    assert!(!runtime.info().remote);
    assert_eq!(
        f.yard.workspace_runtime_state("local").unwrap().state,
        "local"
    );
    assert!(runtime.preview_port().is_ok());
    assert!(matches!(
        f.yard.workspace_runtime("local"),
        Err(Error::Running(_))
    ));
    let out = runtime
        .exec(vec![
            "/bin/sh".into(),
            "-c".into(),
            "test -f a.txt && printf helper-ok".into(),
        ])
        .unwrap()
        .output(1024, Duration::from_secs(10))
        .unwrap();
    assert_eq!(out, b"helper-ok");
    f.yard.workspace_runtime("local").unwrap().close().unwrap();
    assert_eq!(f.yard.branch("local").unwrap().info().session, session);
}

#[test]
fn a_real_pty_accepts_input_and_resize_and_close_releases_the_lease() {
    let f = Fixture::new();
    branch(&f, "pty");
    let terminal = terminal(&f, "pty");
    terminal.resize(37, 91).unwrap();
    terminal
        .write(b"stty -echo; test -t 0 && printf '\\nPTY_%s\\n' true; stty size\n")
        .unwrap();
    wait::until("PTY reports terminal input and changed dimensions", || {
        let page = terminal.read_after(0, 65536).unwrap();
        let text = String::from_utf8_lossy(&page.data);
        text.contains("PTY_true") && text.contains("37 91")
    });
    let pid = terminal_pid(&f, "pty", &terminal);
    assert!(terminal.resize(0, 80).is_err());
    assert!(terminal.write(&vec![b'x'; 16385]).is_err());
    assert!(terminal.read_after(u64::MAX, 1).is_err());
    terminal.close();
    terminal.close();
    assert!(terminal.is_closed());
    assert!(branchyard::services::Reclaim::process(pid, true).is_none());
    f.yard.workspace_runtime("pty").unwrap().close().unwrap();
}

#[test]
fn a_revoked_terminal_closes_without_another_client_request() {
    let f = Fixture::new();
    branch(&f, "revoke");
    let allowed = Arc::new(AtomicBool::new(true));
    let authority = allowed.clone();
    let terminal = Terminal::start(
        f.yard.workspace_runtime("revoke").unwrap(),
        TerminalOptions {
            python: "/usr/bin/python3".into(),
            ..TerminalOptions::default()
        },
        Arc::new(move || {
            if authority.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(Error::Denied("removed member".into()))
            }
        }),
    )
    .unwrap();
    let pid = terminal_pid(&f, "revoke", &terminal);
    allowed.store(false, Ordering::Release);
    wait::until("revocation closes idle terminal", || terminal.is_closed());
    assert!(terminal.write(b"echo denied\n").is_err());
    wait::until("terminal lease released after revocation", || {
        f.yard
            .workspace_runtime("revoke")
            .map(|runtime| runtime.close())
            .is_ok()
    });
    assert!(branchyard::services::Reclaim::process(pid, true).is_none());
}

#[test]
fn output_replay_reports_eviction_and_invalid_limits() {
    let f = Fixture::new();
    branch(&f, "buffer");
    let terminal = terminal(&f, "buffer");
    terminal.write(b"stty -echo; head -c 1200000 /dev/zero | tr '\\000' x; printf '\\nBUFFER_%s\\n' done\n").unwrap();
    wait::until("large PTY output evicts old bytes", || {
        terminal.read_after(0, 65536).unwrap().truncated
    });
    let page = terminal.read_after(0, 65536).unwrap();
    assert!(page.truncated);
    assert!(page.data.len() <= 65536);
    assert!(terminal.read_after(0, 0).is_err());
    terminal.close();
}

#[test]
fn remote_helpers_use_the_retained_provider_and_park_again() {
    let f = Fixture::new();
    let provider = remote(&f, "remote");
    let runtime = f.yard.workspace_runtime("remote").unwrap();
    assert!(runtime.info().remote);
    let allocation = runtime.info().allocation.clone().unwrap();
    let out = runtime
        .exec(vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf remote-ok".into(),
        ])
        .unwrap()
        .output(1024, Duration::from_secs(10))
        .unwrap();
    assert_eq!(out, b"remote-ok");
    assert!(provider.ops().iter().any(|op|matches!(op,FakeOp::Exec {sandbox,argv,..} if sandbox==&allocation && argv.last().is_some_and(|a|a=="printf remote-ok"))));
    assert!(provider
        .sandboxes()
        .iter()
        .any(|(name, state)| name == &allocation && *state == SandboxState::Paused));
}

#[test]
fn lifecycle_actions_change_real_provider_state_and_repeat_safely() {
    let f = Fixture::new();
    let provider = remote(&f, "lifecycle");
    assert_eq!(
        f.yard.workspace_runtime_state("lifecycle").unwrap().state,
        "paused"
    );
    let resumed = f
        .yard
        .workspace_runtime_action("lifecycle", RuntimeAction::Resume, "op-wake")
        .unwrap();
    assert_eq!(resumed.state, "running");
    let count = provider.ops().len();
    assert_eq!(
        f.yard
            .workspace_runtime_action("lifecycle", RuntimeAction::Resume, "op-wake")
            .unwrap(),
        resumed
    );
    assert_eq!(provider.ops().len(), count);
    assert!(f
        .yard
        .workspace_runtime_action("lifecycle", RuntimeAction::Pause, "op-wake")
        .is_err());
    assert_eq!(
        f.yard
            .workspace_runtime_action("lifecycle", RuntimeAction::Pause, "op-sleep")
            .unwrap()
            .state,
        "paused"
    );
    assert_eq!(
        f.yard
            .workspace_runtime_action("lifecycle", RuntimeAction::Suspend, "op-stop")
            .unwrap()
            .state,
        "stopped"
    );
    assert_eq!(
        f.yard
            .workspace_runtime_action("lifecycle", RuntimeAction::Resume, "op-again")
            .unwrap()
            .state,
        "running"
    );
    assert_eq!(
        f.yard
            .workspace_runtime_action("lifecycle", RuntimeAction::Destroy, "op-archive")
            .unwrap()
            .state,
        "absent"
    );
    assert!(f.yard.branch("lifecycle").is_ok());
    assert!(!provider
        .sandboxes()
        .iter()
        .any(|(_, state)| *state == SandboxState::Running));
}

#[test]
fn missing_remote_state_never_becomes_local_or_a_fresh_allocation() {
    let f = Fixture::new();
    let provider = remote(&f, "gone");
    let names = provider
        .sandboxes()
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    for name in names {
        provider.vanish(&name);
    }
    let before = provider
        .ops()
        .iter()
        .filter(|op| matches!(op, FakeOp::Ensure { .. }))
        .count();
    assert!(f.yard.workspace_runtime("gone").is_err());
    assert_eq!(
        provider
            .ops()
            .iter()
            .filter(|op| matches!(op, FakeOp::Ensure { .. }))
            .count(),
        before
    );
    assert!(provider.sandboxes().is_empty());
}

#[test]
fn helper_output_is_bounded_and_local_resource_actions_are_honest() {
    let f = Fixture::new();
    branch(&f, "bounded");
    assert!(f
        .yard
        .workspace_runtime_action("bounded", RuntimeAction::Pause, "pause")
        .is_err());
    assert!(f
        .yard
        .workspace_runtime_action("bounded", RuntimeAction::Pause, "../bad")
        .is_err());
    let output = f
        .yard
        .workspace_runtime("bounded")
        .unwrap()
        .exec(vec!["/bin/sh".into(), "-c".into(), "printf abcdef".into()])
        .unwrap()
        .output(3, Duration::from_secs(10));
    assert!(output.is_err());
    f.yard
        .workspace_runtime("bounded")
        .unwrap()
        .close()
        .unwrap();
}

#[test]
fn a_shared_store_never_moves_a_foreign_workers_local_terminal_here() {
    let f = Fixture::new();
    branch(&f, "worker");
    let worktree = f.yard.branch("worker").unwrap().info().worktree.clone();
    assert!(worktree.is_dir());
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    db.execute("UPDATE steps SET intent = json_set(intent, '$.host', 'another-worker-boot') WHERE branch = 'worker' AND step = 'start'",[]).unwrap();
    let state = f.yard.workspace_runtime_state("worker").unwrap();
    assert_eq!(state.state, "unavailable");
    assert_eq!(
        state.runtime.owner_host.as_deref(),
        Some("another-worker-boot")
    );
    assert!(!state.can_pause && !state.can_resume && !state.can_destroy);
    assert!(f.yard.workspace_runtime("worker").is_err());
    db.execute("UPDATE steps SET intent = json_remove(intent, '$.host') WHERE branch = 'worker' AND step = 'start'",[]).unwrap();
    assert!(f.yard.workspace_runtime("worker").is_err());
    assert_eq!(
        f.yard.workspace_runtime_state("worker").unwrap().state,
        "unavailable"
    );
}

#[test]
fn a_lifecycle_action_does_not_act_after_the_retention_owner_claimed_it() {
    let f = Fixture::new();
    let provider = remote(&f, "claimed");
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    db.execute_batch("CREATE TRIGGER claim_before_runtime_action BEFORE INSERT ON steps WHEN NEW.step LIKE 'workspace-runtime:%' BEGIN DELETE FROM sandboxes WHERE branch = 'claimed' AND kind = 'kept'; END;").unwrap();
    let before = provider.ops().len();
    assert!(f
        .yard
        .workspace_runtime_action("claimed", RuntimeAction::Resume, "claim-race")
        .is_err());
    assert_eq!(provider.ops().len(), before);
}

#[test]
fn previews_read_the_owned_provider_service_and_reject_redirects_and_large_bodies() {
    let f = Fixture::new();
    let provider = remote(&f, "app");
    let app = MockHttp::start(|request| match request.path.as_str() {
        "/redirect" => Response::new(302, "").header("Location", "http://example.invalid"),
        "/large" => Response::new(200, vec![b'x'; 1024 * 1024 + 1]),
        _ => Response::new(200, "workspace-app")
            .header("Content-Type", "text/plain")
            .header("Set-Cookie", "secret=upstream")
            .header("Authorization", "upstream-secret"),
    });
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    db.execute(
        "INSERT INTO ports (port, branch, reserved_ms) VALUES (?1, 'app', 0)",
        [app.port()],
    )
    .unwrap();
    let runtime = f.yard.workspace_runtime("app").unwrap();
    let port = runtime.preview_port().unwrap();
    let allocation = runtime.info().allocation.clone().unwrap();
    let response = runtime.preview(port, "GET", "/assets?v=1").unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"workspace-app");
    assert_eq!(response.headers.len(), 1);
    assert_eq!(response.headers["content-type"], "text/plain");
    let requests = app.await_requests(1);
    assert_eq!(requests[0].path, "/assets?v=1");
    assert_eq!(
        requests[0].header("host"),
        Some(format!("127.0.0.1:{}", app.port()).as_str())
    );
    assert!(requests[0].header("authorization").is_none());
    assert!(requests[0].header("cookie").is_none());
    assert!(provider.ops().iter().any(|op| matches!(op,
        FakeOp::Exec { sandbox, argv, .. }
        if sandbox == &allocation && argv.last().is_some_and(|value| value == "GET")
    )));
    for path in ["/redirect", "/large"] {
        let runtime = f.yard.workspace_runtime("app").unwrap();
        let port = runtime.preview_port().unwrap();
        assert!(runtime.preview(port, "GET", path).is_err());
    }
    assert_eq!(app.await_requests(3).len(), 3);
    app.finish();
}

#[test]
fn helper_stdout_eof_does_not_kill_work_still_waiting_for_input() {
    let f = Fixture::new();
    branch(&f, "finish");
    let worktree = f.yard.branch("finish").unwrap().info().worktree.clone();
    let mut process = f.yard.workspace_runtime("finish").unwrap().exec(vec![
        "/bin/sh".into(), "-c".into(),
        "exec 1>&-; printf ready > helper-ready; read line; printf '%s' \"$line\" > helper-done".into(),
    ]).unwrap();
    let mut input = process.take_stdin().unwrap();
    let output = std::thread::spawn(move || process.output(1024, Duration::from_secs(10)));
    wait::until("helper has closed stdout and is waiting for input", || {
        worktree.join("helper-ready").exists()
    });
    input.write_all(b"finished\n").unwrap();
    drop(input);
    assert!(output.join().unwrap().unwrap().is_empty());
    assert_eq!(
        std::fs::read(worktree.join("helper-done")).unwrap(),
        b"finished"
    );
}
