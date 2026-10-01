//! The recipe provider against a recipe whose scripts make a local "VM":
//! a directory, reached through `tests/fixtures/fake-ssh`, which runs each
//! "remote" command here in a session of its own, as sshd would. No cloud,
//! no sshd. Requires `sh`, `python3`, `ps` and `awk`.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use branchyard_recipe::{Recipe, RecipeProvider};
use branchyard_sandbox::conformance::{self, Setup};
use branchyard_sandbox::{
    admit, ExecSpec, Operation, ProviderError, Requirements, SandboxProvider, SandboxSpec,
    SandboxState, SnapshotGuarantee,
};

const FAKE_SSH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake-ssh");

const CREATE: &str = r#"#!/bin/sh
set -e
vm="$BRANCHYARD_ROOT/.vms/$BRANCHYARD_RECIPE_INSTANCE"
mkdir -p "$vm"
echo running >"$vm/state"
echo "creating $BRANCHYARD_RECIPE_INSTANCE" >&2
printf '{"schemaVersion":1,"connection":{"type":"ssh","target":{"host":"vm.test","port":2222,"username":"dev"},"projectRoot":"%s"},"env":{"VM_NAME":"%s"},"userData":{"dir":"%s"}}\n' \
  "$BRANCHYARD_ROOT/work" "$BRANCHYARD_RECIPE_INSTANCE" "$vm"
"#;

const SUSPEND: &str = r#"#!/bin/sh
vm="$BRANCHYARD_ROOT/.vms/$BRANCHYARD_RECIPE_INSTANCE"
cat >"$vm/suspend.json"
echo suspended >"$vm/state"
"#;

const RESUME: &str = r#"#!/bin/sh
vm="$BRANCHYARD_ROOT/.vms/$BRANCHYARD_RECIPE_INSTANCE"
cat >"$vm/resume.json"
echo running >"$vm/state"
printf '{"schemaVersion":1,"connection":{"type":"ssh","target":{"host":"vm.test","port":2222,"username":"dev"},"projectRoot":"%s"},"env":{"VM_NAME":"%s","RESUMED":"1"}}\n' \
  "$BRANCHYARD_ROOT/work" "$BRANCHYARD_RECIPE_INSTANCE"
"#;

const DESTROY: &str = r#"#!/bin/sh
cat >"$BRANCHYARD_ROOT/.vms/$BRANCHYARD_RECIPE_INSTANCE.destroyed"
rm -rf "$BRANCHYARD_ROOT/.vms/$BRANCHYARD_RECIPE_INSTANCE"
"#;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    ssh: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir_all(root.join("work")).unwrap();
        fs::create_dir_all(root.join("vm-home")).unwrap();
        fs::create_dir_all(root.join("scripts")).unwrap();
        for (name, text) in [
            ("create.sh", CREATE),
            ("suspend.sh", SUSPEND),
            ("resume.sh", RESUME),
            ("destroy.sh", DESTROY),
        ] {
            let path = root.join("scripts").join(name);
            fs::write(&path, text).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        // The ssh program the provider runs: the fake, with the "VM"'s home
        // and only its host.
        let log = root.join("ssh.log");
        let ssh = root.join("ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\nFAKE_SSH_HOME='{}' FAKE_SSH_HOSTS=vm.test FAKE_SSH_LOG='{}' exec \
                 python3 '{FAKE_SSH}' \"$@\"\n",
                root.join("vm-home").display(),
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        Fixture {
            _dir: dir,
            root,
            ssh,
            log,
        }
    }

    fn recipe(&self, pause: bool) -> Recipe {
        let mut recipe = Recipe::new("vm", &self.root, "./scripts/create.sh")
            .with_destroy(Some("./scripts/destroy.sh"));
        if pause {
            recipe.suspend = Some("./scripts/suspend.sh".into());
            recipe.resume = Some("./scripts/resume.sh".into());
        }
        recipe
    }

    fn provider(&self, pause: bool) -> RecipeProvider {
        RecipeProvider::new(self.recipe(pause), self.ssh.display().to_string())
    }
}

fn run(provider: &dyn SandboxProvider, name: &str, argv: &[&str]) -> (Option<i32>, String, String) {
    let spec = ExecSpec {
        argv: argv.iter().map(|s| s.to_string()).collect(),
        ..ExecSpec::default()
    };
    let mut process = provider.exec(name, &spec).unwrap();
    drop(process.take_stdin());
    let mut out = String::new();
    let mut err = String::new();
    process
        .take_stdout()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    process
        .take_stderr()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    (process.wait().unwrap().code, out, err)
}

#[test]
fn the_recipe_provider_passes_sandbox_conformance() {
    let fixture = Fixture::new();
    let provider = fixture.provider(true);
    let setup = Setup::without_mounts("conf", fixture.root.join("work"));
    conformance::run_all(&provider, &setup);
    // Every machine the checks made was destroyed by its script.
    let left: Vec<_> = fs::read_dir(fixture.root.join(".vms"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !n.ends_with(".destroyed"))
        .collect();
    assert!(left.is_empty(), "machines left: {left:?}");
}

#[test]
fn capabilities_are_what_the_recipe_declares_and_admission_holds_to_them() {
    let fixture = Fixture::new();
    let with = fixture.provider(true).capabilities();
    let without = fixture.provider(false).capabilities();
    assert!(with.exec && with.pause && !with.live_branch && with.checkpoint.is_empty());
    assert!(without.exec && !without.pause);
    let pause = Requirements {
        exec: true,
        pause: true,
        ..Requirements::default()
    };
    assert_eq!(admit(&pause, &with), Ok(()));
    let missing = admit(&pause, &without).unwrap_err();
    assert_eq!(missing[0].operation, Operation::Pause);
    let snapshot = Requirements {
        checkpoint: Some(SnapshotGuarantee {
            scope: branchyard_sandbox::SnapshotScope::Disk,
            consistency: branchyard_sandbox::Consistency::Crash,
            locality: branchyard_sandbox::Locality::SameHost,
        }),
        ..Requirements::default()
    };
    assert!(
        admit(&snapshot, &with).is_err(),
        "nothing it does not declare"
    );
    let provider = fixture.provider(false);
    provider.ensure(&SandboxSpec::new("plain")).unwrap();
    assert!(matches!(
        provider.pause("plain"),
        Err(ProviderError::Unsupported(_))
    ));
    provider.destroy("plain").unwrap();
}

#[test]
fn a_machine_is_created_paused_resumed_and_destroyed_by_its_scripts() {
    let fixture = Fixture::new();
    let provider = fixture.provider(true);
    let info = provider.ensure(&SandboxSpec::new("box-1")).unwrap();
    assert_eq!(info.state, SandboxState::Running);
    let vm = fixture.root.join(".vms/box-1");
    assert_eq!(fs::read_to_string(vm.join("state")).unwrap(), "running\n");
    // The result's env and projectRoot reach every exec, over ssh to its
    // target.
    let (code, out, _) = run(
        &provider,
        "box-1",
        &["sh", "-c", "echo \"$VM_NAME $(pwd)\""],
    );
    assert_eq!(code, Some(0));
    assert_eq!(
        out,
        format!("box-1 {}\n", fixture.root.join("work").display())
    );
    let log = fs::read_to_string(&fixture.log).unwrap();
    assert!(
        log.contains("\"-p\", \"2222\", \"-l\", \"dev\", \"--\", \"vm.test\""),
        "{log}"
    );
    assert!(log.contains("BatchMode=yes"), "{log}");

    provider.pause("box-1").unwrap();
    assert_eq!(
        provider.inspect("box-1").unwrap().unwrap().state,
        SandboxState::Paused
    );
    assert_eq!(fs::read_to_string(vm.join("state")).unwrap(), "suspended\n");
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(vm.join("suspend.json")).unwrap()).unwrap();
    assert_eq!(payload["mode"], "suspend");
    assert_eq!(payload["instance"], "box-1");
    assert_eq!(
        payload["recipeResult"]["userData"]["dir"],
        vm.display().to_string()
    );
    let refused = provider.exec(
        "box-1",
        &ExecSpec {
            argv: vec!["true".into()],
            ..ExecSpec::default()
        },
    );
    assert!(matches!(refused, Err(ProviderError::Invalid(why)) if why.contains("paused")));
    provider.pause("box-1").unwrap();

    let resumed = provider.resume("box-1").unwrap();
    assert_eq!(resumed.state, SandboxState::Running);
    let (_, out, _) = run(&provider, "box-1", &["sh", "-c", "echo $RESUMED"]);
    assert_eq!(out, "1\n", "resume's result replaces create's");

    provider.destroy("box-1").unwrap();
    assert!(!vm.exists());
    let payload: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(fixture.root.join(".vms/box-1.destroyed")).unwrap(),
    )
    .unwrap();
    assert_eq!(payload["mode"], "destroy");
    assert_eq!(payload["recipe"], "vm");
    assert_eq!(provider.inspect("box-1").unwrap(), None);
    provider.destroy("box-1").unwrap();
}

#[test]
fn specs_it_cannot_honor_and_failing_scripts_are_refused() {
    let fixture = Fixture::new();
    let provider = fixture.provider(false);
    for spec in [
        SandboxSpec::new("m").mount(branchyard_sandbox::Mount::writable("/tmp", "/w")),
        SandboxSpec::new("m").image("alpine"),
        SandboxSpec::new("m").resources(branchyard_sandbox::Resources {
            cpus: Some(2),
            memory_mib: None,
        }),
    ] {
        assert!(matches!(
            provider.ensure(&spec),
            Err(ProviderError::Invalid(_))
        ));
    }
    assert_eq!(provider.inspect("m").unwrap(), None);
    fs::write(
        fixture.root.join("scripts/create.sh"),
        "#!/bin/sh\necho 'no quota left' >&2\nexit 3\n",
    )
    .unwrap();
    let failed = provider.ensure(&SandboxSpec::new("q")).unwrap_err();
    assert!(
        failed
            .to_string()
            .contains("create exited with code 3: no quota left"),
        "{failed}"
    );
    fs::write(
        fixture.root.join("scripts/create.sh"),
        "#!/bin/sh\necho ready\n",
    )
    .unwrap();
    let failed = provider.ensure(&SandboxSpec::new("q")).unwrap_err();
    assert!(failed.to_string().contains("one JSON object"), "{failed}");
    assert_eq!(provider.inspect("q").unwrap(), None);
}

#[test]
fn a_state_directory_lets_another_process_find_the_machine() {
    let fixture = Fixture::new();
    let state = fixture.root.join("state");
    let first = fixture.provider(true).with_state_dir(&state);
    first.ensure(&SandboxSpec::new("kept").persist()).unwrap();
    first.pause("kept").unwrap();
    drop(first);
    let second = fixture.provider(true).with_state_dir(&state);
    assert_eq!(
        second.inspect("kept").unwrap().unwrap().state,
        SandboxState::Paused
    );
    second.resume("kept").unwrap();
    let (_, out, _) = run(&second, "kept", &["sh", "-c", "echo $VM_NAME"]);
    assert_eq!(out, "kept\n");
    second.destroy("kept").unwrap();
    assert!(!state.join("kept.json").exists());
    assert!(Path::new(&fixture.root.join(".vms/kept.destroyed")).exists());
}
