//! `SubstrateProvider` against the provider contract and its own promises,
//! over real gRPC, a router and real bridge processes, all in the fake
//! cluster. Not evidence about a Substrate cluster.

mod common;

use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;
use std::time::Duration;

use branchyard_sandbox::conformance::{self, Setup};
use branchyard_sandbox::{
    Consistency, ExecSpec, Locality, Mount, ProviderError, SandboxProvider, SandboxSpec,
    SandboxState, SnapshotGuarantee, SnapshotScope,
};
use branchyard_substrate::{Config, Quiesce, SubstrateProvider};
use common::{wait_exec, wait_gone, Cluster, Scratch};

fn setup(name: &str, scratch: &Scratch) -> Setup {
    let workspace = scratch.path("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut setup = Setup::without_mounts(format!("conf-{}", name.replace('_', "-")), workspace);
    setup
        .env
        .insert("PATH".into(), std::env::var_os("PATH").unwrap_or_default());
    setup.timeout = Duration::from_secs(20);
    setup
}

macro_rules! conformance {
    ($($check:ident),* $(,)?) => {$(
        #[test]
        fn $check() {
            let cluster = Cluster::start(stringify!($check));
            let provider = cluster.provider();
            let scratch = Scratch::new(stringify!($check));
            conformance::$check(&provider, &setup(stringify!($check), &scratch));
            assert!(cluster.fake.actor_names().is_empty(), "a sandbox was left behind");
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

fn sh(cwd: &Path, script: &str) -> ExecSpec {
    ExecSpec {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        cwd: cwd.to_path_buf(),
        env: [("PATH".into(), std::env::var_os("PATH").unwrap_or_default())].into(),
    }
}

fn run(provider: &SubstrateProvider, name: &str, script: &str) -> Result<String, ProviderError> {
    let mut process = provider.exec(name, &sh(Path::new("/"), script))?;
    drop(process.take_stdin());
    let mut out = String::new();
    process
        .take_stdout()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let status = process.wait()?;
    assert!(status.success(), "{script}: {status}");
    Ok(out)
}

fn is_refused(result: Result<String, ProviderError>) -> bool {
    matches!(result, Err(ProviderError::Io(e)) if e.kind() == io::ErrorKind::PermissionDenied)
}

#[test]
fn capabilities_declare_exec_through_the_bridge() {
    let cluster = Cluster::start("caps");
    let caps = cluster.provider().capabilities();
    assert!(caps.exec && caps.ingress);
    assert_eq!(caps.branch[0].scope, SnapshotScope::Full);
    let missing = Config::new(
        cluster.fake.endpoint(),
        common::ATESPACE,
        "no-such-template",
        cluster.fake.router(),
    );
    let provider = SubstrateProvider::connect(missing).unwrap();
    assert_eq!(provider.capabilities(), Default::default());
    assert!(provider.template_capabilities().is_err());
}

#[test]
fn specs_it_cannot_honor_are_refused_and_create_nothing() {
    let cluster = Cluster::start("refuse");
    let provider = cluster.provider();
    for spec in [
        SandboxSpec::new("imaged").image("alpine:3"),
        SandboxSpec::new("limited").resources(branchyard_sandbox::Resources {
            cpus: Some(1),
            memory_mib: None,
        }),
        SandboxSpec::new("mounted").mount(Mount::writable("/tmp", "/workspace")),
    ] {
        assert!(
            matches!(provider.ensure(&spec), Err(ProviderError::Invalid(_))),
            "{spec:?}"
        );
    }
    assert!(cluster.fake.actor_names().is_empty());
    let bad_router = Config::new(cluster.fake.endpoint(), "a", "t", "http://router/");
    assert!(matches!(
        SubstrateProvider::connect(bad_router),
        Err(ProviderError::Invalid(_))
    ));
}

#[test]
fn each_attempt_has_its_own_credential_and_ended_ones_stay_dead() {
    let cluster = Cluster::start("attempts");
    let provider = cluster.provider();
    provider.ensure(&SandboxSpec::new("actor")).unwrap();
    let first = provider.endpoint("actor").unwrap();
    assert_eq!(run(&provider, "actor", "echo one").unwrap(), "one\n");

    // A new attempt supersedes the first; the first's credential is dead.
    provider.begin_attempt("actor", "turn-2").unwrap();
    assert_eq!(run(&provider, "actor", "echo two").unwrap(), "two\n");
    let stale = first
        .exec(&sh(Path::new("/"), "true"))
        .map(|_| String::new());
    assert!(is_refused(stale), "a superseded credential was accepted");

    // Ending the attempt kills its processes and refuses its credential.
    let second = provider.endpoint("actor").unwrap();
    let mut sleeper = second
        .exec(&sh(Path::new("/"), "sleep 300 & echo $!; wait"))
        .unwrap();
    let mut line = String::new();
    BufReader::new(branchyard_sandbox::Process::take_stdout(&mut sleeper).unwrap())
        .read_line(&mut line)
        .unwrap();
    let pid: u32 = line.trim().parse().unwrap();
    wait_exec(pid, "sleep");
    let killed = provider.end_attempt("actor").unwrap();
    assert!(killed.iter().any(|n| n == "sleep"), "{killed:?}");
    wait_gone(pid);
    let ended = second
        .exec(&sh(Path::new("/"), "true"))
        .map(|_| String::new());
    assert!(is_refused(ended), "an ended credential was accepted");
    assert!(matches!(
        run(&provider, "actor", "true"),
        Err(ProviderError::Runtime(_))
    ));

    // Suspend and resume restart the bridge; nothing old is revived.
    provider.stop("actor").unwrap();
    provider.ensure(&SandboxSpec::new("actor")).unwrap();
    assert_eq!(run(&provider, "actor", "echo three").unwrap(), "three\n");
    for old in [&first, &second] {
        let revived = old.exec(&sh(Path::new("/"), "true")).map(|_| String::new());
        assert!(is_refused(revived));
    }
    let bridge = cluster.fake.bridge_pid("actor").unwrap();
    provider.destroy("actor").unwrap();
    wait_gone(bridge);
    assert!(cluster.fake.actor_names().is_empty());
}

#[test]
fn a_branch_has_a_new_identity_and_never_accepts_its_parents_credential() {
    let cluster = Cluster::start("branch");
    let provider = cluster.provider();
    provider.ensure(&SandboxSpec::new("parent")).unwrap();

    let full = SnapshotGuarantee {
        scope: SnapshotScope::Full,
        consistency: Consistency::Crash,
        locality: Locality::Portable,
    };
    // A checkpoint needs a stopped actor.
    assert!(matches!(
        provider.checkpoint("parent", &full),
        Err(ProviderError::Runtime(_))
    ));
    let application = SnapshotGuarantee {
        consistency: Consistency::Application,
        ..full
    };
    assert!(matches!(
        provider.checkpoint("parent", &application),
        Err(ProviderError::Unsupported(_))
    ));
    provider.stop("parent").unwrap();
    let checkpoint = provider.checkpoint("parent", &full).unwrap();
    assert_eq!(checkpoint.guarantee, full);

    let child = provider
        .branch(&checkpoint, &SandboxSpec::new("child"))
        .unwrap();
    assert_eq!(child.state, SandboxState::Stopped);
    provider.ensure(&SandboxSpec::new("child")).unwrap();
    assert_eq!(run(&provider, "child", "echo child").unwrap(), "child\n");
    let parent_handle = provider.handle("parent").unwrap().unwrap();
    let child_handle = provider.handle("child").unwrap().unwrap();
    assert_ne!(parent_handle.uid, child_handle.uid);

    // The parent's live credential, presented to the child's bridge, whose
    // attempt state was copied from the parent.
    provider.ensure(&SandboxSpec::new("parent")).unwrap();
    let parent = provider.endpoint("parent").unwrap();
    assert_eq!(run(&provider, "parent", "echo parent").unwrap(), "parent\n");
    let url = provider.config().router_url("child");
    let at_child = parent.with_url(&url).unwrap();
    let crossed = at_child
        .exec(&sh(Path::new("/"), "true"))
        .map(|_| String::new());
    assert!(is_refused(crossed));
    provider.destroy("child").unwrap();
    provider.destroy("parent").unwrap();
}

/// Sandbox-level branching without a live fork: pause and resume through
/// `PauseActor`/`ResumeActor`, a checkpoint of a paused actor suspends then
/// tags it, a branch is created stopped from the tag and resumed, the
/// source stays where it was, live branching is refused, and a released
/// checkpoint's tag is gone.
#[test]
fn pause_resume_and_suspend_tag_create_without_a_live_fork() {
    let cluster = Cluster::start("pause");
    let provider = cluster.provider();
    let caps = provider.capabilities();
    assert!(caps.has(branchyard_sandbox::PAUSE));
    assert!(!caps.has(branchyard_sandbox::LIVE_BRANCH));
    provider.ensure(&SandboxSpec::new("warm")).unwrap();
    run(&provider, "warm", "echo before > state.txt").unwrap();

    provider.pause("warm").unwrap();
    assert_eq!(
        provider.inspect("warm").unwrap().unwrap().state,
        SandboxState::Paused
    );
    // Its attempt ended with the pause.
    assert!(provider.endpoint("warm").is_err());
    provider.pause("warm").unwrap();
    provider.resume("warm").unwrap();
    assert_eq!(
        provider.inspect("warm").unwrap().unwrap().state,
        SandboxState::Running
    );
    assert_eq!(run(&provider, "warm", "echo resumed").unwrap(), "resumed\n");

    provider.pause("warm").unwrap();
    let committed = caps.checkpoint[0];
    let checkpoint = provider.checkpoint("warm", &committed).unwrap();
    assert_eq!(checkpoint.guarantee, committed);
    // Suspended, not paused, by the checkpoint.
    assert_eq!(
        provider.inspect("warm").unwrap().unwrap().state,
        SandboxState::Stopped
    );
    assert!(cluster.fake.tag_names().contains(&checkpoint.reference));

    assert!(matches!(
        &provider.branch_live("warm", &[SandboxSpec::new("live")])[0],
        Err(ProviderError::Unsupported(_))
    ));
    let child = provider
        .branch(&checkpoint, &SandboxSpec::new("forked"))
        .unwrap();
    assert_eq!(
        child.state,
        SandboxState::Stopped,
        "not live: created stopped"
    );
    provider.resume("forked").unwrap();
    assert_eq!(run(&provider, "forked", "echo child").unwrap(), "child\n");
    assert_eq!(
        provider.inspect("warm").unwrap().unwrap().state,
        SandboxState::Stopped,
        "the source stays suspended"
    );
    provider.resume("warm").unwrap();
    assert_eq!(run(&provider, "warm", "echo again").unwrap(), "again\n");

    provider.release_checkpoint(&checkpoint).unwrap();
    assert!(!cluster.fake.tag_names().contains(&checkpoint.reference));
    provider.release_checkpoint(&checkpoint).unwrap();
    for name in ["forked", "warm"] {
        provider.destroy(name).unwrap();
    }
    assert!(cluster.fake.actor_names().is_empty());
}

#[test]
fn a_name_reused_by_another_actor_is_never_acted_on() {
    let cluster = Cluster::start("reuse");
    let first = cluster.provider();
    first.ensure(&SandboxSpec::new("worker")).unwrap();
    // Someone else deletes and recreates the name.
    let other = cluster.provider();
    other.destroy("worker").unwrap();
    other.ensure(&SandboxSpec::new("worker")).unwrap();
    assert!(matches!(
        first.inspect("worker"),
        Err(ProviderError::Runtime(why)) if why.contains("is now")
    ));
    assert!(first.destroy("worker").is_err());
    assert_eq!(
        other.inspect("worker").unwrap().unwrap().state,
        SandboxState::Running
    );
    other.destroy("worker").unwrap();
}

#[test]
fn an_actor_replaced_during_a_call_is_reported_not_passed_off() {
    let cluster = Cluster::start("replaced-during");
    let provider = cluster.provider();
    let replaced = |result: Result<(), ProviderError>, operation: &str| match result {
        Err(ProviderError::Runtime(why)) => {
            assert!(
                why.contains(&format!("replaced during {operation}")),
                "{why}"
            )
        }
        other => panic!("{operation}: expected a replacement error, got {other:?}"),
    };

    // Suspend: the actor is swapped under its name as the call arrives.
    provider.ensure(&SandboxSpec::new("suspended")).unwrap();
    cluster.fake.replace_before("SuspendActor", "suspended");
    replaced(provider.stop("suspended"), "SuspendActor");

    // Resume, when an existing actor is started again.
    provider.ensure(&SandboxSpec::new("resumed")).unwrap();
    provider.stop("resumed").unwrap();
    cluster.fake.replace_before("ResumeActor", "resumed");
    replaced(
        provider.ensure(&SandboxSpec::new("resumed")).map(|_| ()),
        "ResumeActor",
    );

    // A tag taken while the actor was replaced is deleted again.
    let full = SnapshotGuarantee {
        scope: SnapshotScope::Full,
        consistency: Consistency::Crash,
        locality: Locality::Portable,
    };
    provider.ensure(&SandboxSpec::new("tagged")).unwrap();
    provider.stop("tagged").unwrap();
    cluster.fake.replace_before("CreateTag", "tagged");
    replaced(
        provider.checkpoint("tagged", &full).map(|_| ()),
        "CreateTag",
    );
    assert!(
        cluster.fake.tag_names().is_empty(),
        "{:?}",
        cluster.fake.tag_names()
    );

    // Without a replacement the same calls succeed, and a replacement
    // before a call is refused before anything is done.
    provider.ensure(&SandboxSpec::new("plain")).unwrap();
    provider.stop("plain").unwrap();
    provider.checkpoint("plain", &full).unwrap();
    let other = cluster.provider();
    other.destroy("plain").unwrap();
    other.ensure(&SandboxSpec::new("plain")).unwrap();
    assert!(matches!(
        provider.stop("plain"),
        Err(ProviderError::Runtime(why)) if why.contains("is now")
    ));
    assert_eq!(
        other.inspect("plain").unwrap().unwrap().state,
        SandboxState::Running
    );
    for name in ["suspended", "resumed", "tagged", "plain"] {
        other.destroy(name).unwrap();
    }
}

#[test]
fn stop_and_checkpoint_wait_for_running_execs_or_refuse() {
    let cluster = Cluster::start("quiesce");
    let provider = cluster.provider();
    provider.ensure(&SandboxSpec::new("busy")).unwrap();
    let full = SnapshotGuarantee {
        scope: SnapshotScope::Full,
        consistency: Consistency::Crash,
        locality: Locality::Portable,
    };
    assert!(provider.status("busy").unwrap().execs.is_empty());

    // A harness in the middle of a tool call: a child at work.
    let mut process = provider
        .exec("busy", &sh(Path::new("/"), "sleep 300 & echo $!; wait"))
        .unwrap();
    let mut line = String::new();
    BufReader::new(process.take_stdout().unwrap())
        .read_line(&mut line)
        .unwrap();
    let sleeper: u32 = line.trim().parse().unwrap();
    wait_exec(sleeper, "sleep");
    let status = provider.status("busy").unwrap();
    assert_eq!(status.execs.len(), 1);
    assert!(status.execs[0].members.contains(&"sleep".to_owned()));

    let busy = |result: Result<(), ProviderError>| match result {
        Err(ProviderError::Io(error)) => {
            assert_eq!(error.kind(), io::ErrorKind::ResourceBusy, "{error}");
            assert!(error.to_string().contains("at work: sh, sleep"), "{error}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    };
    busy(
        provider
            .checkpoint_with("busy", &full, Quiesce::Refuse)
            .map(|_| ()),
    );
    busy(provider.stop_with("busy", Quiesce::Wait(Duration::from_millis(300))));
    // Nothing was stopped: the exec still runs and the actor is up.
    assert!(common::alive(sleeper));
    assert_eq!(
        provider.inspect("busy").unwrap().unwrap().state,
        SandboxState::Running
    );

    // Once the work ends, a waiting checkpoint proceeds.
    let finisher = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        // SAFETY: plain syscall on the test's own sleeper.
        unsafe { libc_kill(sleeper) };
        process.wait().unwrap()
    });
    let checkpoint = provider
        .checkpoint_with("busy", &full, Quiesce::Wait(Duration::from_secs(20)))
        .unwrap();
    assert!(finisher.join().unwrap().success());
    assert_eq!(checkpoint.sandbox, "busy");
    assert_eq!(
        provider.inspect("busy").unwrap().unwrap().state,
        SandboxState::Stopped
    );

    // Forced, it proceeds regardless and ends what runs.
    provider.ensure(&SandboxSpec::new("busy")).unwrap();
    let mut process = provider
        .exec("busy", &sh(Path::new("/"), "sleep 300 & echo $!; wait"))
        .unwrap();
    let mut line = String::new();
    BufReader::new(process.take_stdout().unwrap())
        .read_line(&mut line)
        .unwrap();
    let sleeper: u32 = line.trim().parse().unwrap();
    provider
        .checkpoint_with("busy", &full, Quiesce::Force)
        .unwrap();
    wait_gone(sleeper);
    assert!(!process.wait().unwrap().success());
    provider.destroy("busy").unwrap();
}

/// SIGTERM to `pid`.
unsafe fn libc_kill(pid: u32) {
    extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    kill(pid as i32, 15);
}
