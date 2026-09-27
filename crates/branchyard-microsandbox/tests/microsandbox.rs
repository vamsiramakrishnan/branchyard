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
