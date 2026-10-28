//! `by run --provider substrate` end to end: the built `by` binary against
//! the fake Substrate cluster in this test process, whose actor runs the
//! fake ACP agent behind a real bridge; then `by merge`. Hermetic: no real
//! harness, no cluster. Requires `git` and `sh`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use branchyard_bridge::Signer;
use branchyard_substrate::fake::FakeCluster;
use branchyard_substrate::pb;
use branchyard_substrate::template::{bridge_template, BridgeTemplate};

const BY: &str = env!("CARGO_BIN_EXE_by");

/// Build `bin` of `package` next to `by`; cargo exposes a binary's path
/// only to its own package's tests.
fn built(package: &str, bin: &str) -> PathBuf {
    let profile_dir = Path::new(BY).parent().unwrap().to_path_buf();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .args(["build", "--quiet", "--offline", "--manifest-path"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"))
        .args(["-p", package, "--bin", bin])
        .env("CARGO_TARGET_DIR", profile_dir.parent().unwrap());
    match profile_dir.file_name().and_then(|n| n.to_str()) {
        Some("debug") => {}
        Some("release") => {
            command.arg("--release");
        }
        Some(other) => {
            command.args(["--profile", other]);
        }
        None => panic!("unexpected binary location {BY}"),
    }
    assert!(command.status().unwrap().success(), "building {bin} failed");
    profile_dir.join(bin)
}

fn run(dir: &Path, program: &str, args: &[&str]) -> Output {
    let mut command = Command::new(program);
    branchyard_testkit::hermetic(&mut command)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("NO_COLOR", "1")
        // Never a person's own ~/.config/branchyard/config.toml.
        .env(
            "BRANCHYARD_USER_CONFIG",
            "/nonexistent/branchyard-config.toml",
        )
        .env("PAGER", "cat");
    command.output().unwrap()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = run(dir, "git", args);
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap()
}

/// A repository, a bridge key and a fake cluster with a bridge template,
/// under a temporary directory.
struct Setup {
    dir: PathBuf,
    root: PathBuf,
    agent: PathBuf,
    fake: FakeCluster,
    workdir: PathBuf,
    home: PathBuf,
}

impl Setup {
    fn new(name: &str) -> Setup {
        // Bridges inherit this process's environment.
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        let agent = built("branchyard-runtime", "fake-acp-agent");
        let bridge = built("branchyard-bridge", "branchyard-bridge");
        let dir = std::env::temp_dir().join(format!(
            "branchyard-cli-substrate-{}-{name}",
            std::process::id()
        ));
        if dir.exists() {
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::create_dir_all(dir.join("repo")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let root = dir.join("repo");
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "user.email", "test@localhost"]);
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "initial"]);

        let keygen = run(
            &dir,
            bridge.to_str().unwrap(),
            &["keygen", "--out", "bridge.key"],
        );
        assert!(keygen.status.success());
        let public_key = String::from_utf8(keygen.stdout).unwrap().trim().to_owned();
        assert_eq!(
            Signer::read(&dir.join("bridge.key")).unwrap().public_key(),
            public_key
        );
        let fake = FakeCluster::start("yard", Some(bridge.clone()), &dir.join("cluster"));
        fake.add_template(bridge_template(
            "yard",
            &BridgeTemplate {
                name: "by-harness".into(),
                image: "registry.example/by@sha256:00".into(),
                bridge: "/usr/local/bin/branchyard-bridge".into(),
                public_key,
                sandbox_class: pb::SandboxClass::Gvisor,
                sandbox_config: "gvisor".into(),
                storage_location: "gs://bucket/by".into(),
            },
        ));
        let workdir = dir.join("actor/workspace");
        let home = dir.join("actor/home");
        fake.fresh_on_create(&workdir);
        fake.fresh_on_create(&home);
        Setup {
            dir,
            root,
            agent,
            fake,
            workdir,
            home,
        }
    }

    fn save_profile(&self, template: &str, key: &str, token: Option<&str>) {
        let config = branchyard::config::ProjectConfig {
            substrate: Some(branchyard::config::Substrate {
                endpoint: self.fake.endpoint().into(),
                router: self.fake.router().into(),
                template: template.into(),
                atespace: "yard".into(),
                key: key.into(),
                api_token_file: token.map(str::to_owned),
                workdir: self.workdir.display().to_string(),
                home: self.home.display().to_string(),
                egress: branchyard::config::SubstrateEgress::Deny,
                ..Default::default()
            }),
            ..Default::default()
        };
        fs::write(
            self.root.join("branchyard.toml"),
            branchyard::config::render(&config, "Hermetic setup"),
        )
        .unwrap();
    }

    fn harness(&self, name: &str, script: &str) -> PathBuf {
        let path = self.dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// The `--provider substrate` flags, with the key at `key`.
    fn flags(&self, key: &str) -> Vec<String> {
        [
            "--provider",
            "substrate",
            "--substrate-endpoint",
            self.fake.endpoint(),
            "--substrate-router",
            self.fake.router(),
            "--substrate-template",
            "by-harness",
            "--substrate-atespace",
            "yard",
            "--substrate-key",
            key,
            "--substrate-workdir",
            self.workdir.to_str().unwrap(),
            "--substrate-home",
            self.home.to_str().unwrap(),
        ]
        .map(str::to_owned)
        .to_vec()
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        branchyard_support::cleanup_dir(&self.dir);
    }
}

#[test]
fn by_run_in_a_substrate_actor_then_merge() {
    let setup = Setup::new("local");
    let (root, agent, fake) = (&setup.root, &setup.agent, &setup.fake);
    let flags = setup.flags("../bridge.key");
    let mut args = vec![
        "run",
        "WRITE hello.txt=hi",
        "--name",
        "hello",
        "--harness",
        "gemini-cli",
        "--command",
        agent.to_str().unwrap(),
    ];
    args.extend(flags.iter().map(String::as_str));
    let out = run(root, BY, &args);
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stderr.contains("Agent Substrate actors"), "{stderr}");
    assert!(
        stdout.contains("wrote hello.txt") && stdout.contains("ready"),
        "{stdout}"
    );
    assert!(
        fake.actor_names().is_empty(),
        "the turn's actor was deleted"
    );

    let merged = run(root, BY, &["merge", "hello"]);
    assert!(
        merged.status.success(),
        "{}",
        String::from_utf8_lossy(&merged.stderr)
    );
    assert_eq!(git(root, &["show", "main:hello.txt"]), "hi\n");
}

#[test]
fn saved_substrate_runs_from_a_subdirectory_and_explicit_local_wins() {
    let setup = Setup::new("saved-config");
    let root = &setup.root;
    setup.fake.require_api_token("hermetic-control-token");
    fs::write(setup.dir.join("control.token"), "hermetic-control-token\n").unwrap();
    let config = branchyard::config::ProjectConfig {
        defaults: branchyard::config::Defaults {
            harness: Some("gemini-cli".into()),
            provider: Some(branchyard::config::ProviderKind::Substrate),
            ..Default::default()
        },
        substrate: Some(branchyard::config::Substrate {
            endpoint: setup.fake.endpoint().into(),
            router: setup.fake.router().into(),
            template: "by-harness".into(),
            atespace: "yard".into(),
            key: "../bridge.key".into(),
            api_token_file: Some("../control.token".into()),
            workdir: setup.workdir.display().to_string(),
            home: setup.home.display().to_string(),
            egress: branchyard::config::SubstrateEgress::Deny,
            ..Default::default()
        }),
        ..Default::default()
    };
    fs::write(
        root.join("branchyard.toml"),
        branchyard::config::render(&config, "Hermetic remote run"),
    )
    .unwrap();
    let subdir = root.join("nested");
    fs::create_dir(&subdir).unwrap();
    let out = run(
        &subdir,
        BY,
        &[
            "run",
            "WRITE saved.txt=remote",
            "--name",
            "saved",
            "--command",
            setup.agent.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("Agent Substrate actors"));
    assert!(setup.fake.actor_names().is_empty());
    let merged = run(root, BY, &["merge", "saved"]);
    assert!(
        merged.status.success(),
        "{}",
        String::from_utf8_lossy(&merged.stderr)
    );
    assert_eq!(git(root, &["show", "main:saved.txt"]), "remote\n");

    fs::remove_file(setup.dir.join("control.token")).unwrap();
    let local = run(
        &subdir,
        BY,
        &[
            "run",
            "WRITE local.txt=local",
            "--name",
            "local",
            "--provider",
            "local",
            "--command",
            setup.agent.to_str().unwrap(),
        ],
    );
    assert!(
        local.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&local.stdout),
        String::from_utf8_lossy(&local.stderr)
    );
    assert!(!String::from_utf8_lossy(&local.stderr).contains("Agent Substrate actors"));
    assert!(setup.fake.actor_names().is_empty());
}

#[test]
fn substrate_setup_is_repeatable_and_smoke_cleans_success_and_failure() {
    let setup = Setup::new("setup-smoke");
    let key = setup.dir.join("new-bridge.key");
    let config = branchyard::config::ProjectConfig {
        substrate: Some(branchyard::config::Substrate {
            endpoint: setup.fake.endpoint().into(),
            router: setup.fake.router().into(),
            template: "by-prepared".into(),
            atespace: "yard".into(),
            key: "../new-bridge.key".into(),
            workdir: setup.workdir.display().to_string(),
            home: setup.home.display().to_string(),
            egress: branchyard::config::SubstrateEgress::Deny,
            ..Default::default()
        }),
        ..Default::default()
    };
    fs::write(
        setup.root.join("branchyard.toml"),
        branchyard::config::render(&config, "Hermetic setup"),
    )
    .unwrap();
    let image = format!("test/bridge@sha256:{}", "a".repeat(64));
    let args = [
        "substrate",
        "setup",
        "--image",
        &image,
        "--storage",
        "s3://test/snapshots",
        "--json",
    ];
    let mut plan_args = args.to_vec();
    plan_args.push("--plan");
    let plan = run(&setup.root, BY, &plan_args);
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    assert!(!key.exists(), "planning generated a signing key");
    assert!(
        !setup.root.join(".branchyard").exists(),
        "planning initialized the repository store"
    );
    let observer =
        branchyard_substrate::SubstrateProvider::connect(branchyard_substrate::Config::new(
            setup.fake.endpoint(),
            "yard",
            "by-prepared",
            setup.fake.router(),
        ))
        .unwrap();
    assert!(
        observer.template_capabilities().is_err(),
        "planning created the template"
    );
    assert!(setup.fake.actor_names().is_empty());

    let mut apply_args = args.to_vec();
    apply_args.push("--yes");
    let applied = run(&setup.root, BY, &apply_args);
    assert!(
        applied.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&applied.stdout),
        String::from_utf8_lossy(&applied.stderr)
    );
    let first: serde_json::Value = serde_json::from_slice(&applied.stdout).unwrap();
    assert_eq!(first["created_template"], true);
    assert!(key.exists());
    let signer = Signer::read(&key).unwrap().public_key();
    let again = run(&setup.root, BY, &apply_args);
    assert!(
        again.status.success(),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    let second: serde_json::Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(second["created_template"], false);
    assert_eq!(Signer::read(&key).unwrap().public_key(), signer);

    let smoke = run(
        &setup.root,
        BY,
        &[
            "substrate",
            "check",
            "--smoke",
            "--harness",
            "true",
            "--json",
        ],
    );
    assert!(
        smoke.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&smoke.stdout),
        String::from_utf8_lossy(&smoke.stderr)
    );
    let checked: serde_json::Value = serde_json::from_slice(&smoke.stdout).unwrap();
    assert_eq!(checked["smoke"], true);
    assert_eq!(checked["model"], "not_checked");
    assert_eq!(checked["template"], "validated");
    assert_eq!(checked["template_details"]["signer"], "matches");
    assert!(setup.fake.actor_names().is_empty());

    let failed = run(
        &setup.root,
        BY,
        &[
            "substrate",
            "check",
            "--smoke",
            "--harness",
            "branchyard-missing-test-harness",
        ],
    );
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr)
        .contains("execute branchyard-missing-test-harness --version in the actor"));
    assert!(
        setup.fake.actor_names().is_empty(),
        "failed harness left its probe actor"
    );
}

#[test]
fn substrate_setup_validates_and_authenticates_before_writing() {
    let setup = Setup::new("setup-readonly");
    let key = setup.dir.join("credentials/new.key");
    setup.save_profile("by-prepared", "../credentials/new.key", None);
    let image = format!("test/bridge@sha256:{}", "b".repeat(64));
    for flags in [
        vec!["--cpu", ""],
        vec!["--memory", " "],
        vec!["--worker-label", "pool=a", "--worker-label", "pool=b"],
    ] {
        let mut args = vec![
            "substrate",
            "setup",
            "--image",
            &image,
            "--storage",
            "s3://test/snapshots",
            "--yes",
        ];
        args.extend(flags);
        let invalid = run(&setup.root, BY, &args);
        assert!(!invalid.status.success());
        assert!(!key.parent().unwrap().exists());
    }
    for mode in ["--plan", "--yes"] {
        let invalid = run(
            &setup.root,
            BY,
            &[
                "substrate",
                "setup",
                "--image",
                "unpinned:latest",
                "--storage",
                "s3://test/snapshots",
                mode,
            ],
        );
        assert!(!invalid.status.success());
        assert!(String::from_utf8_lossy(&invalid.stderr).contains("sha256"));
        assert!(!key.parent().unwrap().exists());
        assert!(!setup.root.join(".branchyard").exists());
    }
    setup.fake.require_api_token("hermetic-setup-token");
    for mode in ["--plan", "--yes"] {
        let refused = run(
            &setup.root,
            BY,
            &[
                "substrate",
                "setup",
                "--image",
                &image,
                "--storage",
                "s3://test/snapshots",
                mode,
            ],
        );
        assert!(!refused.status.success());
        assert!(
            String::from_utf8_lossy(&refused.stderr)
                .to_ascii_lowercase()
                .contains("missing or incorrect api bearer token"),
            "{}",
            String::from_utf8_lossy(&refused.stderr)
        );
        assert!(
            !key.parent().unwrap().exists(),
            "authentication failure wrote a key directory"
        );
        assert!(!setup.root.join(".branchyard").exists());
    }
    fs::write(setup.dir.join("control.token"), "hermetic-setup-token").unwrap();
    setup.save_profile(
        "by-prepared",
        "../credentials/new.key",
        Some("../control.token"),
    );
    let plan = run(
        &setup.root,
        BY,
        &[
            "substrate",
            "setup",
            "--image",
            &image,
            "--storage",
            "s3://test/snapshots",
            "--plan",
            "--json",
        ],
    );
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let planned: serde_json::Value = serde_json::from_slice(&plan.stdout).unwrap();
    assert_eq!(planned["key"]["action"], "create");
    assert_eq!(planned["plan"]["template"], "yard/by-prepared");
    assert!(!key.parent().unwrap().exists());
    assert!(!setup.root.join(".branchyard").exists());

    // A missing key cannot silently claim an existing immutable template.
    setup.save_profile(
        "by-harness",
        "../credentials/new.key",
        Some("../control.token"),
    );
    let conflict = run(
        &setup.root,
        BY,
        &[
            "substrate",
            "setup",
            "--image",
            &image,
            "--storage",
            "s3://test/snapshots",
            "--yes",
        ],
    );
    assert!(!conflict.status.success());
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("immutable template differs"));
    assert!(!key.parent().unwrap().exists());
    assert!(setup.fake.actor_names().is_empty());
}

#[test]
fn substrate_check_inspects_contract_and_drains_both_streams() {
    let setup = Setup::new("check-contract");
    setup.save_profile("by-harness", "../bridge.key", None);
    let mut template = bridge_template(
        "yard",
        &BridgeTemplate {
            name: "by-harness".into(),
            image: format!("test/bridge@sha256:{}", "c".repeat(64)),
            bridge: "/usr/local/bin/branchyard-bridge".into(),
            public_key: Signer::read(&setup.dir.join("bridge.key"))
                .unwrap()
                .public_key(),
            sandbox_class: pb::SandboxClass::Gvisor,
            sandbox_config: "gvisor".into(),
            storage_location: "s3://test/snapshots".into(),
        },
    );
    let valid = template.clone();
    template.containers[0]
        .wakeup_probe
        .as_mut()
        .unwrap()
        .http_get
        .as_mut()
        .unwrap()
        .port = 8080;
    setup.fake.add_template(template);
    let invalid = run(&setup.root, BY, &["substrate", "check"]);
    assert!(!invalid.status.success());
    assert!(
        String::from_utf8_lossy(&invalid.stderr).contains("port 80"),
        "{}",
        String::from_utf8_lossy(&invalid.stderr)
    );
    assert!(setup.fake.actor_names().is_empty());
    let mut wrong_key = valid.clone();
    let (other, _) = Signer::generate().unwrap();
    wrong_key.containers[0]
        .env
        .iter_mut()
        .find(|env| env.name == branchyard_bridge::KEY_ENV)
        .unwrap()
        .value = other.public_key();
    setup.fake.add_template(wrong_key);
    let mismatch = run(&setup.root, BY, &["substrate", "check"]);
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("does not match"));
    assert!(setup.fake.actor_names().is_empty());
    setup.fake.add_template(valid);

    let noisy = setup.harness(
        "noisy-harness",
        "awk 'BEGIN { for (i=0; i<20000; i++) print \"diagnostic\" }' >&2\nprintf 'harness 1.0\\n'",
    );
    let output = run(
        &setup.root,
        BY,
        &[
            "substrate",
            "check",
            "--smoke",
            "--harness",
            noisy.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["harness_version"], "harness 1.0");
    assert!(setup.fake.actor_names().is_empty());

    let stderr_version = setup.harness("stderr-harness", "printf 'stderr version 2.0\\n' >&2");
    let output = run(
        &setup.root,
        BY,
        &[
            "substrate",
            "check",
            "--smoke",
            "--harness",
            stderr_version.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["harness_version"], "stderr version 2.0");
    assert!(setup.fake.actor_names().is_empty());
    let verbose = setup.harness(
        "verbose-harness",
        "awk 'BEGIN { for (i=0; i<20000; i++) print \"verbose version\" }'",
    );
    let output = run(
        &setup.root,
        BY,
        &[
            "substrate",
            "check",
            "--smoke",
            "--harness",
            verbose.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let captured = report["harness_version"].as_str().unwrap();
    assert!(captured.len() < 66_000);
    assert!(captured.contains("output truncated"));
    assert!(setup.fake.actor_names().is_empty());
    assert!(!setup.root.join(".branchyard").exists());
}

#[test]
fn substrate_smoke_cancellation_removes_the_actor() {
    let setup = Setup::new("smoke-cancel");
    setup.save_profile("by-harness", "../bridge.key", None);
    let marker = setup.dir.join("harness-started");
    let harness = setup.harness(
        "blocking-harness",
        &format!("touch '{}'\nexec tail -f /dev/null", marker.display()),
    );
    for signal in [rustix::process::Signal::INT, rustix::process::Signal::TERM] {
        if marker.exists() {
            fs::remove_file(&marker).unwrap();
        }
        let mut child = branchyard_testkit::hermetic(&mut Command::new(BY))
            .current_dir(&setup.root)
            .args([
                "substrate",
                "check",
                "--smoke",
                "--harness",
                harness.to_str().unwrap(),
            ])
            .env(
                "BRANCHYARD_USER_CONFIG",
                "/nonexistent/branchyard-config.toml",
            )
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        branchyard_testkit::wait::until("the smoke harness to start", || marker.exists());
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(child.id() as i32).unwrap(),
            signal,
        )
        .unwrap();
        branchyard_testkit::wait::until("cancelled smoke to exit", || child.try_wait().unwrap());
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("smoke cancelled"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("removed"));
        assert!(
            setup.fake.actor_names().is_empty(),
            "cancelled smoke leaked its actor"
        );
    }
}

#[test]
fn substrate_smoke_reports_cleanup_failure_with_the_actor_name() {
    let setup = Setup::new("smoke-cleanup-failure");
    setup.fake.require_api_token("smoke-control-token");
    let token = setup.dir.join("control.token");
    fs::write(&token, "smoke-control-token").unwrap();
    setup.save_profile("by-harness", "../bridge.key", Some("../control.token"));
    let marker = setup.dir.join("harness-started");
    let harness = setup.harness(
        "blocking-harness",
        &format!("touch '{}'\nexec tail -f /dev/null", marker.display()),
    );
    let mut child = branchyard_testkit::hermetic(&mut Command::new(BY))
        .current_dir(&setup.root)
        .args([
            "substrate",
            "check",
            "--smoke",
            "--harness",
            harness.to_str().unwrap(),
        ])
        .env(
            "BRANCHYARD_USER_CONFIG",
            "/nonexistent/branchyard-config.toml",
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    branchyard_testkit::wait::until("the smoke harness to start", || marker.exists());
    let actors = setup.fake.actor_names();
    assert_eq!(actors.len(), 1);
    setup.fake.require_api_token("rotated-control-token");
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(child.id() as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    branchyard_testkit::wait::until("cancelled smoke to report failed cleanup", || {
        child.try_wait().unwrap()
    });
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("smoke cancelled"), "{error}");
    assert!(error.contains("cleanup also failed"), "{error}");
    assert!(error.contains(&actors[0]), "{error}");
    assert!(
        !error.contains("disposable actor") || !error.contains("removed"),
        "{error}"
    );
    assert_eq!(setup.fake.actor_names(), actors);

    // Restore access and complete the failed cleanup without leaking a bridge.
    setup.fake.require_api_token("smoke-control-token");
    let mut config = branchyard_substrate::Config::new(
        setup.fake.endpoint(),
        "yard",
        "by-harness",
        setup.fake.router(),
    );
    config.api_token_file = Some(token);
    let provider = branchyard_substrate::SubstrateProvider::connect(config).unwrap();
    branchyard_sandbox::SandboxProvider::destroy(&provider, &actors[0]).unwrap();
    assert!(setup.fake.actor_names().is_empty());
}

/// `by serve` on an ephemeral loopback port, with its data in
/// `<dir>/<name>`; killed on drop.
struct Served {
    child: std::process::Child,
    url: String,
    token: PathBuf,
}

impl Served {
    fn start(dir: &Path, name: &str, root: &Path, extra: &[&str]) -> Served {
        use std::io::BufRead;
        let data = dir.join(name);
        let mut child = branchyard_testkit::hermetic(&mut Command::new(BY))
            .current_dir(dir)
            .args(["serve", "--listen", "127.0.0.1:0", "--quiet", "--repo"])
            .arg(format!("app={}", root.display()))
            .arg("--data-dir")
            .arg(&data)
            .args(extra)
            .env(
                "BRANCHYARD_USER_CONFIG",
                "/nonexistent/branchyard-config.toml",
            )
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let url = line
            .trim()
            .strip_prefix("listening on ")
            .unwrap()
            .to_owned();
        Served {
            child,
            url,
            token: data.join("token"),
        }
    }

    fn by(&self, dir: &Path, args: &[String]) -> Output {
        let token = self.token.display().to_string();
        let mut all = vec!["--remote", &self.url, "--token-file", &token];
        all.extend(args.iter().map(String::as_str));
        run(dir, BY, &all)
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        branchyard_support::best_effort("kill the test child", self.child.kill());
        branchyard_support::best_effort("reap the test child", self.child.wait());
    }
}

#[test]
fn by_remote_run_in_a_substrate_actor_on_the_server() {
    let setup = Setup::new("remote");
    let (dir, agent) = (&setup.dir, &setup.agent);
    let harness = format!("gemini-cli={}", agent.display());
    let key = dir.join("bridge.key");
    let run_args = |name: &str, key: &str| {
        let mut args: Vec<String> = ["run", "WRITE hello.txt=hi", "--name", name]
            .map(str::to_owned)
            .to_vec();
        args.extend(["--harness", "gemini-cli", "--yes"].map(str::to_owned));
        args.extend(setup.flags(key));
        args
    };

    // Refused by a server whose operator did not allow the provider.
    let plain = Served::start(dir, "plain", &setup.root, &["--harness-command", &harness]);
    let refused = plain.by(dir, &run_args("refused", key.to_str().unwrap()));
    assert_eq!(refused.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("--allow-provider substrate"), "{stderr}");
    drop(plain);

    let server = Served::start(
        dir,
        "allowed",
        &setup.root,
        &[
            "--harness-command",
            &harness,
            "--allow-provider",
            "substrate",
        ],
    );
    let out = server.by(dir, &run_args("hello", key.to_str().unwrap()));
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stderr.contains("Agent Substrate actors"), "{stderr}");
    assert!(
        stdout.contains("wrote hello.txt") && stdout.contains("ready"),
        "{stdout}"
    );
    assert!(
        setup.fake.actor_names().is_empty(),
        "the turn's actor was deleted"
    );
    let merged = server.by(dir, &["merge".into(), "hello".into()]);
    assert!(
        merged.status.success(),
        "{}",
        String::from_utf8_lossy(&merged.stderr)
    );
    assert_eq!(git(&setup.root, &["show", "main:hello.txt"]), "hi\n");

    // A relative key would name a file on this machine, not the server.
    let relative = server.by(dir, &run_args("relative", "bridge.key"));
    assert_eq!(relative.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&relative.stderr).contains("names a file on the server"),
        "{}",
        String::from_utf8_lossy(&relative.stderr)
    );
}
