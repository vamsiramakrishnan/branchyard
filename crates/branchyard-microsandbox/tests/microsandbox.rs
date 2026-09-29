//! The Microsandbox provider against a real runtime. Every test is
//! `#[ignore]`: it needs Linux with KVM, the `msb` 0.7.3 runtime and a
//! toolchain of Rust 1.94 or newer. See `docs/providers.md` for setup, then:
//!
//! ```text
//! BY_MSB_IMAGE=alpine:3.20 cargo +1.94 test -p branchyard-microsandbox \
//!     --features microsandbox -- --ignored --test-threads 1
//! ```
//!
//! `BY_MSB_IMAGE` must have `sh`, `sleep`, `cat`, `printf` and `nproc`;
//! it defaults to `alpine:3.20`.

#![cfg(feature = "microsandbox")]

use std::path::PathBuf;
use std::time::Duration;

use branchyard_microsandbox::plan::DISK_SNAPSHOT;
use branchyard_microsandbox::MicrosandboxProvider;
use branchyard_sandbox::conformance::{self, Setup};
use branchyard_sandbox::{
    ExecSpec, Mount, Process, Resources, SandboxProvider, SandboxSpec, SandboxState,
};

const GUEST: &str = "/workspace";

fn image() -> String {
    std::env::var("BY_MSB_IMAGE").unwrap_or_else(|_| "alpine:3.20".into())
}

fn workspace(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("msb-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn setup(name: &str) -> Setup {
    let mut setup = Setup::new(format!("by-msb-{name}"), workspace(name), GUEST);
    setup.image = Some(image());
    setup.resources = Resources {
        cpus: Some(1),
        memory_mib: Some(512),
    };
    setup.timeout = Duration::from_secs(60);
    setup
}

macro_rules! conformance {
    ($($check:ident),* $(,)?) => {$(
        #[test]
        #[ignore = "needs Linux with KVM and the msb 0.7.3 runtime"]
        fn $check() {
            let provider = MicrosandboxProvider::new().unwrap();
            conformance::$check(&provider, &setup(stringify!($check)));
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
);

fn run(provider: &dyn SandboxProvider, sandbox: &str, script: &str) -> (Option<i32>, String) {
    let spec = ExecSpec {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        cwd: GUEST.into(),
        env: Default::default(),
    };
    let mut process: Box<dyn Process> = provider.exec(sandbox, &spec).unwrap();
    drop(process.take_stdin());
    let mut out = String::new();
    std::io::Read::read_to_string(&mut process.take_stdout().unwrap(), &mut out).unwrap();
    (process.wait().unwrap().code, out)
}

fn spec(setup: &Setup, name: &str) -> SandboxSpec {
    SandboxSpec::new(name)
        .image(image())
        .resources(setup.resources.clone())
        .mount(Mount::writable(&setup.workspace, GUEST))
}

#[test]
#[ignore = "needs Linux with KVM and the msb 0.7.3 runtime"]
fn limits_reach_the_guest() {
    let setup = setup("limits");
    let provider = MicrosandboxProvider::new().unwrap();
    let name = "by-msb-limits";
    let _ = provider.destroy(name);
    provider.ensure(&spec(&setup, name)).unwrap();
    let (code, cpus) = run(&provider, name, "nproc");
    assert_eq!((code, cpus.trim()), (Some(0), "1"));
    let (_, kib) = run(&provider, name, "awk '/MemTotal/ {print $2}' /proc/meminfo");
    let kib: u64 = kib.trim().parse().unwrap();
    assert!(
        kib <= 512 * 1024,
        "MemTotal {kib} KiB exceeds the 512 MiB limit"
    );
    provider.destroy(name).unwrap();
}

#[test]
#[ignore = "needs Linux with KVM and the msb 0.7.3 runtime"]
fn root_disk_writes_are_private_to_each_sandbox() {
    let setup = setup("private");
    let provider = MicrosandboxProvider::new().unwrap();
    let (a, b) = ("by-msb-private-a", "by-msb-private-b");
    for name in [a, b] {
        let _ = provider.destroy(name);
        provider.ensure(&spec(&setup, name)).unwrap();
    }
    assert_eq!(run(&provider, a, "echo a > /root/private").0, Some(0));
    let (code, _) = run(&provider, b, "cat /root/private");
    assert_ne!(code, Some(0), "b saw a's root-disk write");
    for name in [a, b] {
        provider.destroy(name).unwrap();
    }
}

#[test]
#[ignore = "needs Linux with KVM and the msb 0.7.3 runtime"]
fn a_disk_checkpoint_branches_into_a_new_sandbox() {
    let setup = setup("branch");
    let provider = MicrosandboxProvider::new().unwrap();
    let (source, child) = ("by-msb-branch-source", "by-msb-branch-child");
    for name in [source, child] {
        let _ = provider.destroy(name);
    }
    provider.ensure(&spec(&setup, source)).unwrap();
    assert_eq!(
        run(&provider, source, "echo kept > /root/marker && sync").0,
        Some(0)
    );
    let checkpoint = provider.checkpoint(source, &DISK_SNAPSHOT).unwrap();
    assert_eq!(checkpoint.guarantee, DISK_SNAPSHOT);
    let branched = provider.branch(&checkpoint, &spec(&setup, child)).unwrap();
    assert_eq!(branched.state, SandboxState::Running);
    let (code, out) = run(&provider, child, "cat /root/marker");
    assert_eq!((code, out.as_str()), (Some(0), "kept\n"));
    // A branch has its own identity: writes do not flow back.
    run(&provider, child, "echo child > /root/marker");
    let (_, source_view) = run(&provider, source, "cat /root/marker");
    assert_eq!(source_view, "kept\n");
    for name in [source, child] {
        provider.destroy(name).unwrap();
    }
}

// Live branching, the pattern mario-never-dies probes: a paused source is
// branched into children that keep its processes (same PIDs), whose writes
// stay private, while the source stays paused. Declared only with
// `with_live_branch(true)`; unqualified until these pass on a KVM host.

fn live_provider() -> MicrosandboxProvider {
    MicrosandboxProvider::new().unwrap().with_live_branch(true)
}

/// A long-running process in `sandbox`, started detached from the exec,
/// and its PID.
fn start_sleeper(provider: &dyn SandboxProvider, sandbox: &str) -> String {
    let (code, pid) = run(
        provider,
        sandbox,
        "sleep 3600 >/dev/null 2>&1 & echo $! > /root/sleeper.pid; cat /root/sleeper.pid",
    );
    assert_eq!(code, Some(0));
    pid.trim().to_owned()
}

#[test]
#[ignore = "needs Linux with KVM and the msb 0.7.3 runtime"]
fn a_live_branch_child_keeps_the_sources_processes() {
    let setup = setup("live-pids");
    let provider = live_provider();
    let (source, child) = ("by-msb-live-source", "by-msb-live-child");
    for name in [source, child] {
        let _ = provider.destroy(name);
    }
    provider.ensure(&spec(&setup, source).persist()).unwrap();
    let pid = start_sleeper(&provider, source);
    provider.pause(source).unwrap();
    let children = [SandboxSpec::new(child).mount(Mount::writable(&setup.workspace, GUEST))];
    let made = provider.branch_live(source, &children);
    assert!(made[0].is_ok(), "{:?}", made[0]);
    let (code, alive) = run(&provider, child, &format!("kill -0 {pid} && echo alive"));
    assert_eq!(
        (code, alive.trim()),
        (Some(0), "alive"),
        "PID {pid} in the child"
    );
    for name in [child, source] {
        provider.destroy(name).unwrap();
    }
}

#[test]
#[ignore = "needs Linux with KVM and the msb 0.7.3 runtime"]
fn a_live_branch_childs_writes_are_private_and_the_paused_source_stays_paused() {
    let setup = setup("live-private");
    let provider = live_provider();
    let source = "by-msb-live-src2";
    let children = ["by-msb-live-c1", "by-msb-live-c2"];
    for name in std::iter::once(source).chain(children) {
        let _ = provider.destroy(name);
    }
    provider.ensure(&spec(&setup, source).persist()).unwrap();
    assert_eq!(
        run(&provider, source, "echo source > /root/marker && sync").0,
        Some(0)
    );
    provider.pause(source).unwrap();
    // Each child gets its own workspace, rebound at the same guest path.
    let specs: Vec<SandboxSpec> = children
        .iter()
        .map(|name| SandboxSpec::new(*name).mount(Mount::writable(workspace(name), GUEST)))
        .collect();
    for made in provider.branch_live(source, &specs) {
        made.unwrap();
    }
    assert_eq!(
        provider.inspect(source).unwrap().unwrap().state,
        SandboxState::Paused,
        "the source stayed paused"
    );
    run(
        &provider,
        children[0],
        "echo c1 > /root/marker && touch /workspace/c1-was-here",
    );
    let (_, other) = run(&provider, children[1], "cat /root/marker");
    assert_eq!(other, "source\n", "c2 saw c1's root-disk write");
    assert!(workspace_file(children[0], "c1-was-here"));
    assert!(!workspace_file(children[1], "c1-was-here"));
    provider.resume(source).unwrap();
    let (_, own) = run(&provider, source, "cat /root/marker");
    assert_eq!(own, "source\n");
    for name in children.into_iter().chain([source]) {
        provider.destroy(name).unwrap();
    }
}

fn workspace_file(name: &str, file: &str) -> bool {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("msb-{name}"))
        .join(file)
        .exists()
}

#[test]
#[ignore = "needs Linux with KVM and the msb 0.7.3 runtime"]
fn a_persisted_sandbox_is_resumed_by_another_provider() {
    let setup = setup("persist");
    let name = "by-msb-persist";
    {
        let provider = live_provider();
        let _ = provider.destroy(name);
        provider.ensure(&spec(&setup, name).persist()).unwrap();
        run(&provider, name, "echo kept > /root/marker");
        provider.pause(name).unwrap();
    }
    // A new provider, as a later `by send` would have.
    let provider = live_provider();
    assert_eq!(
        provider.inspect(name).unwrap().unwrap().state,
        SandboxState::Paused
    );
    provider.resume(name).unwrap();
    let (code, out) = run(&provider, name, "cat /root/marker");
    assert_eq!((code, out.as_str()), (Some(0), "kept\n"));
    provider.destroy(name).unwrap();
}
