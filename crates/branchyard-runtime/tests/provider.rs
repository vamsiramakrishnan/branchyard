//! The local provider against the provider contract, and sessions started
//! through a provider.

#![cfg(unix)]
#![allow(clippy::unwrap_in_result, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use branchyard_harness::SessionMode;
use branchyard_runtime::{LocalProvider, Session};
use branchyard_sandbox::conformance::{self, Setup};
use branchyard_sandbox::{
    admit, Capabilities, ExecSpec, Mount, Process, ProviderError, Requirements, Resources,
    SandboxInfo, SandboxProvider, SandboxSpec, SandboxState,
};
use branchyard_testkit::wait;
use common::{workdir, WAIT};

/// A setup whose workspace is a fresh directory; local mounts are identity.
fn setup(name: &str) -> Setup {
    let workspace = workdir(&format!("provider-{name}")).canonicalize().unwrap();
    let mut setup = Setup::new(format!("local-{name}"), &workspace, &workspace);
    setup
        .env
        .insert("PATH".into(), std::env::var_os("PATH").unwrap_or_default());
    setup.timeout = Duration::from_secs(10);
    setup
}

macro_rules! conformance {
    ($($check:ident),* $(,)?) => {$(
        #[test]
        fn $check() {
            conformance::$check(&LocalProvider::new(), &setup(stringify!($check)));
        }
    )*};
}

conformance!(
    lifecycle,
    exit_status,
    missing_program,
    stdio_round_trip,
    env_and_cwd,
    workspace_mount,
    read_only_mount,
    kill_reaches_descendants,
    teardown_names_survivors,
    drop_tears_down,
    stop_ends_processes,
    egress_confinement,
);

/// The local provider, except that the background child of
/// `teardown_names_survivors`'s script stays a forked, un-exec'd `sh` until
/// the second exec after it: the interleaving a loaded scheduler produces
/// when it runs the shell to exit before its child has exec'd `sleep`.
struct ExecLate {
    local: LocalProvider,
    flag: PathBuf,
    execs: AtomicUsize,
}

impl SandboxProvider for ExecLate {
    fn capabilities(&self) -> Capabilities {
        self.local.capabilities()
    }
    fn ensure(&self, spec: &SandboxSpec) -> Result<SandboxInfo, ProviderError> {
        self.local.ensure(spec)
    }
    fn inspect(&self, name: &str) -> Result<Option<SandboxInfo>, ProviderError> {
        self.local.inspect(name)
    }
    fn exec(&self, name: &str, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError> {
        let mut spec = spec.clone();
        if spec
            .argv
            .last()
            .is_some_and(|script| script == "sleep 300 & echo $!")
        {
            // Builtins only, so the child stays `sh` and alone in the group.
            let script = format!(
                "{{ while [ ! -e '{}' ]; do :; done; exec sleep 300; }} & echo $!",
                self.flag.display()
            );
            *spec.argv.last_mut().unwrap() = script;
            self.execs.store(1, Ordering::SeqCst);
        } else if self.execs.load(Ordering::SeqCst) > 0
            && self.execs.fetch_add(1, Ordering::SeqCst) == 2
        {
            std::fs::write(&self.flag, "").unwrap();
        }
        self.local.exec(name, &spec)
    }
    fn stop(&self, name: &str) -> Result<(), ProviderError> {
        self.local.stop(name)
    }
    fn destroy(&self, name: &str) -> Result<(), ProviderError> {
        self.local.destroy(name)
    }
}

/// Teardown names a background child it finds before the exec `sh`, so
/// `teardown_names_survivors` must wait for the exec rather than assume it.
#[test]
fn teardown_names_survivors_waits_for_the_exec() {
    let setup = setup("teardown-exec-late");
    let provider = ExecLate {
        local: LocalProvider::new(),
        flag: setup.workspace.join("exec"),
        execs: AtomicUsize::new(0),
    };
    conformance::teardown_names_survivors(&provider, &setup);
    assert!(provider.flag.exists(), "the script was not slowed");
}

#[test]
fn the_local_provider_declares_exec_only() {
    let capabilities = LocalProvider::new().capabilities();
    let exec = Requirements {
        exec: true,
        ..Requirements::default()
    };
    assert_eq!(admit(&exec, &capabilities), Ok(()));
    let ingress = Requirements {
        ingress: true,
        ..Requirements::default()
    };
    assert!(admit(&ingress, &capabilities).is_err());
    let provider = LocalProvider::new();
    let guarantee = branchyard_sandbox::SnapshotGuarantee {
        scope: branchyard_sandbox::SnapshotScope::Disk,
        consistency: branchyard_sandbox::Consistency::Crash,
        locality: branchyard_sandbox::Locality::SameHost,
    };
    assert!(matches!(
        provider.checkpoint("x", &guarantee),
        Err(ProviderError::Unsupported(_))
    ));
}

#[test]
fn the_local_provider_refuses_what_it_cannot_honor() {
    let provider = LocalProvider::new();
    let dir = PathBuf::from("/tmp/by-local");
    let refused = [
        SandboxSpec::new("image").image("alpine:3"),
        SandboxSpec::new("limits").resources(Resources {
            cpus: Some(1),
            memory_mib: None,
        }),
        SandboxSpec::new("remap").mount(Mount::writable(&dir, "/workspace")),
        SandboxSpec::new("read-only").mount(Mount::read_only(&dir, &dir)),
    ];
    for spec in refused {
        let refusal = provider.ensure(&spec);
        assert!(
            matches!(refusal, Err(ProviderError::Invalid(_))),
            "{}: {refusal:?}",
            spec.name
        );
        assert_eq!(provider.inspect(&spec.name).unwrap(), None);
    }
    let missing = provider.exec("nowhere", &ExecSpec::default());
    assert!(matches!(missing, Err(ProviderError::NotFound(_))));
}

#[test]
fn stop_kills_background_children_and_blocks_exec() {
    let setup = setup("stop-children");
    let provider = LocalProvider::new();
    let spec = SandboxSpec::new("local-stop-children")
        .mount(Mount::writable(&setup.workspace, &setup.workspace));
    provider.ensure(&spec).unwrap();
    let mut env = setup.env.clone();
    env.insert("X".into(), "1".into());
    let exec = ExecSpec {
        argv: vec!["sh".into(), "-c".into(), "sleep 300 & echo $!; wait".into()],
        cwd: setup.workspace.clone(),
        env,
    };
    let mut process = provider.exec(&spec.name, &exec).unwrap();
    let mut line = String::new();
    let mut stdout = std::io::BufReader::new(process.take_stdout().unwrap());
    std::io::BufRead::read_line(&mut stdout, &mut line).unwrap();
    let child: u32 = line.trim().parse().unwrap();
    assert!(wait::alive(child));
    provider.stop(&spec.name).unwrap();
    process.wait().unwrap();
    wait::gone(child);
    assert_eq!(
        provider.inspect(&spec.name).unwrap().unwrap().state,
        SandboxState::Stopped
    );
    assert!(matches!(
        provider.exec(&spec.name, &exec),
        Err(ProviderError::Runtime(_))
    ));
    provider.ensure(&spec).unwrap();
    let mut again = provider.exec(&spec.name, &exec).unwrap();
    again.kill().unwrap();
    again.teardown();
    provider.destroy(&spec.name).unwrap();
}

#[test]
fn a_session_runs_through_a_provider() {
    let dir = workdir("session-in-provider").canonicalize().unwrap();
    let provider = LocalProvider::new();
    let spec = SandboxSpec::new("local-session").mount(Mount::writable(&dir, &dir));
    provider.ensure(&spec).unwrap();
    let env: BTreeMap<_, _> = [("HOME".into(), dir.join("home").into_os_string())].into();
    let mut session = Session::start_in(
        common::agent(),
        common::open(&dir, SessionMode::Fresh),
        &provider,
        &spec.name,
        env,
        None,
    )
    .unwrap();
    session.wait_ready(WAIT).unwrap();
    let report = session
        .run_turn(
            "hi",
            &mut |_| branchyard_harness::PermissionDecision::Allow,
            WAIT,
        )
        .unwrap();
    assert_eq!(report.text, "echo: hi");
    let closed = session.close(WAIT).unwrap();
    assert!(closed.survivors.is_empty(), "{:?}", closed.survivors);

    let error = Session::start_in(
        common::agent(),
        common::open(&dir, SessionMode::Fresh),
        &provider,
        "no-such-sandbox",
        BTreeMap::new(),
        None,
    )
    .unwrap_err();
    assert!(
        matches!(&error, branchyard_runtime::RuntimeError::Spawn { source, .. } if source.kind() == std::io::ErrorKind::NotFound),
        "{error}"
    );
    provider.destroy(&spec.name).unwrap();
}
