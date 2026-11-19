use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use branchyard_sandbox::{
    ExecSpec, Mount, ProviderError, Resources, SandboxProvider, SandboxSpec, SandboxState,
};
use branchyard_testkit::{processes_in, wait, Scratch};
use serde_json::Value;

use crate::{validate_spec, Config, ContainerProvider, Network, Runtime};

struct Fixture {
    scratch: Scratch,
    provider: ContainerProvider,
}

impl Fixture {
    fn new() -> Result<Self, ProviderError> {
        let scratch = Scratch::new("container");
        let binary = scratch.path().join("docker");
        std::os::unix::fs::symlink(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/fake_docker.py"),
            &binary,
        )?;
        let provider = ContainerProvider::new(Config::default(), scratch.path().join("receipts"))?
            .with_binary(binary);
        Ok(Self { scratch, provider })
    }

    fn mode(&self, mode: &str) -> Result<(), ProviderError> {
        fs::write(self.scratch.path().join("mode"), mode)?;
        Ok(())
    }

    fn calls(&self) -> Result<Vec<Vec<String>>, Box<dyn std::error::Error>> {
        fs::read_to_string(self.scratch.path().join("calls.jsonl"))?
            .lines()
            .map(|line| serde_json::from_str(line).map_err(Into::into))
            .collect()
    }

    fn receipt_path(&self) -> PathBuf {
        self.scratch
            .path()
            .join("receipts")
            .join(format!("{}.json", blake3::hash(b"test")))
    }
}

fn spec() -> SandboxSpec {
    SandboxSpec::new("test")
        .image(format!("example/worker@sha256:{}", "a".repeat(64)))
        .resources(Resources {
            cpus: Some(2),
            memory_mib: Some(128),
        })
}

#[test]
fn only_pinned_copied_ephemeral_positive_specs_are_admitted() {
    assert!(validate_spec(&spec()).is_ok());
    for invalid in [
        SandboxSpec::new("test"),
        SandboxSpec::new("test").image("worker:latest"),
        spec().mount(Mount::writable("/host", "/workspace")),
        spec().persist(),
        spec().resources(Resources {
            cpus: Some(0),
            memory_mib: None,
        }),
        spec().resources(Resources {
            cpus: None,
            memory_mib: Some(0),
        }),
    ] {
        assert!(validate_spec(&invalid).is_err());
    }
}

#[test]
fn transport_credentials_are_omitted_before_guest_ack() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fixture.provider.ensure(&spec())?;
    fixture.mode("exec-credentials")?;
    let exec = ExecSpec {
        argv: vec!["true".into()],
        cwd: "/workspace".into(),
        env: Default::default(),
        writable: None,
    };
    let failure = match fixture.provider.exec("test", &exec) {
        Ok(_) => return Err("credential-bearing transport unexpectedly started".into()),
        Err(error) => error.to_string(),
    };
    assert!(
        failure.contains("Docker transport diagnostic omitted"),
        "{failure}"
    );
    assert!(!failure.contains("private-credential"), "{failure}");
    fixture.mode("")?;
    fixture.provider.destroy("test")?;
    Ok(())
}

#[test]
fn an_owned_allocation_is_inspected_reused_stopped_restarted_and_removed(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let provider = &fixture.provider;
    assert_eq!(provider.inspect("absent")?, None);
    assert_eq!(provider.ensure(&spec())?.state, SandboxState::Running);
    assert_eq!(provider.ensure(&spec())?.state, SandboxState::Running);
    let receipt: Value = serde_json::from_slice(&fs::read(fixture.receipt_path())?)?;
    assert_eq!(receipt["id"], "c".repeat(64));
    assert_eq!(receipt["volumes"].as_array().map(Vec::len), Some(3));
    assert_eq!(
        fs::metadata(fixture.receipt_path())?.permissions().mode() & 0o777,
        0o600
    );
    provider.stop("test")?;
    assert_eq!(
        provider.inspect("test")?.map(|i| i.state),
        Some(SandboxState::Stopped)
    );
    provider.ensure(&spec())?;
    provider.destroy("test")?;
    provider.destroy("test")?;
    assert_eq!(provider.inspect("test")?, None);
    assert!(!fixture.receipt_path().exists());
    let calls = fixture.calls()?;
    assert_eq!(
        calls
            .iter()
            .filter(|c| c.get(3).is_some_and(|s| s == "create"))
            .count(),
        1
    );
    let create = calls
        .iter()
        .find(|c| c.get(3).is_some_and(|s| s == "create"))
        .ok_or("no create")?;
    for flag in [
        "--read-only",
        "--cap-drop",
        "--security-opt",
        "--memory",
        "--memory-swap",
        "--cpus",
        "--pids-limit",
    ] {
        assert!(create.iter().any(|s| s == flag), "{flag}");
    }
    // The tool tree has a volume of its own at the guest root, beside the worktree and the home.
    for destination in ["/workspace", "/branchyard/home", "/tools"] {
        let wanted = format!("type=volume,destination={destination}");
        assert!(create.contains(&wanted), "{destination}");
    }
    assert!(!provider.capabilities().egress);
    assert!(!provider.capabilities().pause);
    Ok(())
}

#[test]
fn creation_gap_is_reconciled_without_creating_twice() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fixture.mode("create-gap")?;
    assert!(fixture.provider.ensure(&spec()).is_err());
    let before: Value = serde_json::from_slice(&fs::read(fixture.receipt_path())?)?;
    assert!(before["id"].is_null());
    fixture.mode("")?;
    fixture.provider.ensure(&spec())?;
    assert_eq!(
        fixture
            .calls()?
            .iter()
            .filter(|c| c.get(3).is_some_and(|s| s == "create"))
            .count(),
        1
    );
    fixture.provider.destroy("test")?;
    Ok(())
}

#[test]
fn recovery_can_destroy_a_creation_without_a_committed_id() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    fixture.mode("create-gap")?;
    assert!(fixture.provider.ensure(&spec()).is_err());
    fixture.mode("")?;
    fixture.provider.destroy("test")?;
    assert!(!fixture.receipt_path().exists());
    Ok(())
}

#[test]
fn daemon_runtime_limits_mounts_and_identity_cannot_drift() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    fixture.provider.ensure(&spec())?;
    for mode in [
        "daemon",
        "no-memory",
        "no-runtime",
        "runtime",
        "memory",
        "owner",
        "replaced",
        "mount",
        "privileged",
        "network",
        "volume-target",
    ] {
        fixture.mode(mode)?;
        assert!(fixture.provider.ensure(&spec()).is_err(), "{mode}");
        assert!(fixture.provider.inspect("test").is_err(), "{mode}");
        assert!(
            fixture.provider.exec("test", &ExecSpec::default()).is_err(),
            "{mode}"
        );
    }
    for mode in ["daemon", "owner", "replaced"] {
        fixture.mode(mode)?;
        assert!(fixture.provider.destroy("test").is_err(), "{mode}");
        assert!(fixture.provider.stop("test").is_err(), "{mode}");
    }
    fixture.mode("")?;
    let changed = spec().resources(Resources {
        cpus: Some(1),
        memory_mib: Some(128),
    });
    assert!(fixture.provider.ensure(&changed).is_err());
    fixture.provider.destroy("test")?;
    Ok(())
}

#[test]
fn startup_and_deletion_failures_keep_the_receipt_for_cleanup(
) -> Result<(), Box<dyn std::error::Error>> {
    for mode in ["start-fail", "ready-fail", "delete-fail"] {
        let fixture = Fixture::new()?;
        fixture.mode(mode)?;
        if mode == "delete-fail" {
            fixture.provider.ensure(&spec())?;
            assert!(fixture.provider.destroy("test").is_err());
        } else {
            assert!(fixture.provider.ensure(&spec()).is_err());
        }
        assert!(fixture.receipt_path().exists());
        fixture.mode("")?;
        fixture.provider.destroy("test")?;
        assert!(!fixture.receipt_path().exists());
    }
    Ok(())
}

#[test]
fn incomplete_volume_cleanup_never_discards_ownership_state(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fixture.provider.ensure(&spec())?;
    fixture.mode("volume-leak")?;
    assert!(fixture.provider.destroy("test").is_err());
    assert!(fixture.provider.destroy("test").is_err());
    assert!(fixture.receipt_path().exists());
    assert!(!fixture
        .calls()?
        .iter()
        .any(|c| c.get(2).is_some_and(|s| s == "volume") && c.get(3).is_some_and(|s| s == "rm")));
    Ok(())
}

#[test]
fn exec_uses_immutable_ids_and_preserves_binary_stdio_and_start_errors(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fixture.provider.ensure(&spec())?;
    let command = ExecSpec {
        argv: vec![
            "sh".into(),
            "-c".into(),
            "cat; printf 'diagnostic\\000\\377' >&2".into(),
        ],
        cwd: fixture.scratch.path().into(),
        ..ExecSpec::default()
    };
    let mut process = fixture.provider.exec("test", &command)?;
    let bytes = b"input\0with\xffbinary\n";
    process.take_stdin().ok_or("no stdin")?.write_all(bytes)?;
    let mut output = Vec::new();
    process
        .take_stdout()
        .ok_or("no stdout")?
        .read_to_end(&mut output)?;
    assert_eq!(output, bytes);
    let mut errors = Vec::new();
    process
        .take_stderr()
        .ok_or("no stderr")?
        .read_to_end(&mut errors)?;
    assert_eq!(errors, b"diagnostic\0\xff");
    assert!(process.wait()?.success());
    process.teardown();
    let missing = ExecSpec {
        argv: vec!["/no-such-command".into()],
        ..command
    };
    assert!(fixture.provider.exec("test", &missing).is_err());
    let calls = fixture.calls()?;
    for call in calls
        .iter()
        .filter(|c| c.get(3).is_some_and(|s| s == "exec"))
    {
        assert!(call.contains(&"c".repeat(64)));
        assert!(!call.iter().any(|s| s.starts_with("branchyard-")));
    }
    fixture.provider.destroy("test")?;
    Ok(())
}

#[test]
fn explicit_runc_and_unrestricted_network_are_not_silently_substituted(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let provider = ContainerProvider::new(
        Config {
            runtime: Runtime::Runc,
            network: Network::Unrestricted,
        },
        fixture.scratch.path().join("other"),
    )?
    .with_binary(fixture.scratch.path().join("docker"));
    provider.ensure(&spec())?;
    let state: Value =
        serde_json::from_slice(&fs::read(fixture.scratch.path().join("docker.json"))?)?;
    assert_eq!(state["container"]["HostConfig"]["Runtime"], "runc");
    assert_eq!(state["container"]["HostConfig"]["NetworkMode"], "bridge");
    assert!(!provider.capabilities().egress);
    provider.destroy("test")?;
    Ok(())
}

#[test]
fn defaults_apply_effective_limits_and_runtime_absence_refuses_creation(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fixture.mode("no-runtime")?;
    assert!(fixture.provider.ensure(&spec()).is_err());
    assert!(!fixture.receipt_path().exists());
    assert!(!fixture
        .calls()?
        .iter()
        .any(|call| call.get(3).is_some_and(|arg| arg == "create")));
    fixture.mode("")?;
    fixture
        .provider
        .ensure(&spec().resources(Resources::default()))?;
    let state: Value =
        serde_json::from_slice(&fs::read(fixture.scratch.path().join("docker.json"))?)?;
    let host = &state["container"]["HostConfig"];
    assert_eq!(host["Runtime"], "runsc");
    assert_eq!(host["NetworkMode"], "none");
    assert_eq!(host["NanoCpus"], 1_000_000_000);
    assert_eq!(host["Memory"], 1024 * 1024 * 1024);
    fixture.provider.destroy("test")?;
    Ok(())
}

#[test]
fn changed_configuration_and_missing_receipts_cannot_control_an_existing_container(
) -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fixture.provider.ensure(&spec())?;
    let changed = ContainerProvider::new(
        Config {
            runtime: Runtime::Runc,
            ..Config::default()
        },
        fixture.scratch.path().join("receipts"),
    )?
    .with_binary(fixture.scratch.path().join("docker"));
    assert!(changed.ensure(&spec()).is_err());
    assert!(changed.inspect("test").is_err());
    assert!(changed.stop("test").is_err());
    assert!(changed.destroy("test").is_err());

    let receipt = fs::read(fixture.receipt_path())?;
    fs::remove_file(fixture.receipt_path())?;
    assert_eq!(fixture.provider.inspect("test")?, None);
    fixture.provider.stop("test")?;
    fixture.provider.destroy("test")?;
    assert!(fixture.provider.exec("test", &ExecSpec::default()).is_err());
    let state: Value =
        serde_json::from_slice(&fs::read(fixture.scratch.path().join("docker.json"))?)?;
    assert_eq!(state["container"]["State"]["Running"], true);
    assert!(!fixture.calls()?.iter().any(|call| {
        call.get(3)
            .is_some_and(|arg| matches!(arg.as_str(), "stop" | "rm"))
    }));
    fs::write(fixture.receipt_path(), receipt)?;
    fixture.provider.destroy("test")?;
    Ok(())
}

#[test]
fn concurrent_ensure_calls_share_one_committed_allocation() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    std::thread::scope(|scope| {
        let first = scope.spawn(|| fixture.provider.ensure(&spec()));
        let second = scope.spawn(|| fixture.provider.ensure(&spec()));
        assert!(first.join().unwrap().is_ok());
        assert!(second.join().unwrap().is_ok());
    });
    assert_eq!(
        fixture
            .calls()?
            .iter()
            .filter(|c| c.get(3).is_some_and(|s| s == "create"))
            .count(),
        1
    );
    fixture.provider.destroy("test")?;
    Ok(())
}

#[test]
fn dropping_an_exec_tears_down_its_remote_process_group() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    fixture.provider.ensure(&spec())?;
    let command = ExecSpec {
        argv: vec![
            "sh".into(),
            "-c".into(),
            "tail -f /dev/null & echo $!; wait".into(),
        ],
        cwd: fixture.scratch.path().into(),
        ..ExecSpec::default()
    };
    let mut process = fixture.provider.exec("test", &command)?;
    let mut pid = String::new();
    // Keep the reader: `tail -f` exits once nothing reads its stdout.
    let mut stdout = BufReader::new(process.take_stdout().ok_or("no stdout")?);
    stdout.read_line(&mut pid)?;
    let pid: u32 = pid.trim().parse()?;
    // The shell prints the pid as it forks `tail`. Wait until the test has
    // seen that process in the scratch directory, so that the later wait for
    // it to be gone cannot pass for a process that was never seen there.
    wait::until(
        "the exec descendant to run in the scratch directory",
        || processes_in(fixture.scratch.path()).contains(&pid),
    );
    drop(process);
    wait::until("the exec descendant to stop", || {
        !processes_in(fixture.scratch.path()).contains(&pid)
    });
    fixture.provider.destroy("test")?;
    Ok(())
}

#[test]
fn commands_have_deadlines_output_bounds_and_redacted_errors() {
    use std::process::Command;
    use std::time::Duration;
    let blocked = crate::docker::bounded(
        Command::new("/usr/bin/python3").args(["-c", "import signal; signal.pause()"]),
        Duration::from_millis(50),
    );
    assert!(blocked.is_err());
    let excessive = crate::docker::bounded(
        Command::new("/usr/bin/python3").args(["-c", "print('x' * 2000000)"]),
        Duration::from_secs(5),
    );
    assert!(excessive.is_err());
    let failed = crate::docker::bounded(
        Command::new("sh").args(["-c", "echo secret-marker >&2; exit 1"]),
        Duration::from_secs(5),
    );
    assert!(!failed.unwrap_err().to_string().contains("secret-marker"));

    let scratch = Scratch::new("container-timeout");
    let inherited = crate::docker::bounded(
        Command::new("/usr/bin/python3")
            .current_dir(scratch.path())
            .args([
                "-c",
                "import os, signal\nif os.fork(): os._exit(0)\nsignal.pause()",
            ]),
        Duration::from_millis(100),
    );
    assert!(inherited.is_err());
    wait::until("timed-out Docker descendants to exit", || {
        processes_in(scratch.path()).is_empty()
    });
}

#[test]
fn missing_or_corrupt_receipts_never_authorize_adoption() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    fixture.provider.ensure(&spec())?;
    let receipt = fs::read(fixture.receipt_path())?;
    fs::write(fixture.receipt_path(), b"not json")?;
    assert!(fixture.provider.inspect("test").is_err());
    assert!(fixture.provider.destroy("test").is_err());
    fs::write(fixture.receipt_path(), &receipt)?;
    let mut docker: Value =
        serde_json::from_slice(&fs::read(fixture.scratch.path().join("docker.json"))?)?;
    docker
        .as_object_mut()
        .ok_or("not an object")?
        .remove("container");
    fs::write(
        fixture.scratch.path().join("docker.json"),
        serde_json::to_vec(&docker)?,
    )?;
    assert_eq!(fixture.provider.inspect("test")?, None);
    assert!(fixture.provider.ensure(&spec()).is_err());
    // Missing containers do not authorize deleting volumes by their mutable names.
    assert!(fixture.provider.destroy("test").is_err());
    Ok(())
}
