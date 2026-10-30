//! Turns on an environment recipe's machine, end to end: a recipe whose
//! scripts make a local "VM" reached through
//! `crates/branchyard-recipe/tests/fixtures/fake-ssh`, which runs each
//! "remote" command here in a session of its own, as sshd would. The fake
//! ACP agent runs there, the worktree and home go in and come back as
//! `cat` and `tar` streams over ssh, the candidate merges; a kept machine
//! is suspended and resumed through the recipe; removal destroys it; and
//! recovery brings back the work of an engine that died and destroys its
//! machine. Hermetic; no sshd or cloud VM. Requires `git`, `sh`, `tar`,
//! `ps` and `python3`.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use branchyard::{
    Activity, BranchStatus, Effort, Policy, Provider, Provisioning, RecipeOptions, SandboxEvent,
    SandboxKeep, SandboxOrigin, SecretSource, TaskOptions, Yard,
};
use branchyard_testkit::wait;
use common::{fake_agent, git, stored_record, text, Fixture};

const FAKE_SSH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../branchyard-recipe/tests/fixtures/fake-ssh"
);

/// `BRANCHYARD_SSH`, set once for this test process: the fake ssh, with
/// the "VM"'s `HOME` in a directory of this process's own.
fn ssh() {
    static SET: OnceLock<()> = OnceLock::new();
    SET.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("branchyard-recipe-ssh-{}", std::process::id()));
        fs::create_dir_all(dir.join("vm-home")).unwrap();
        let wrapper = dir.join("ssh");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nFAKE_SSH_HOME='{}' FAKE_SSH_HOSTS=vm.test exec python3 '{FAKE_SSH}' \
                 \"$@\"\n",
                dir.join("vm-home").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("BRANCHYARD_SSH", &wrapper);
    });
}

/// The recipe's scripts, in the fixture's directory: each appends its mode
/// and instance to `recipe.log`; `create` and `resume` print an ssh
/// connection to `vm.test` with `CI=1` in the result's env.
fn recipe(f: &Fixture) -> RecipeOptions {
    ssh();
    let scripts = f.dir.join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    let log = f.dir.join("recipe.log");
    let result = format!(
        r#"{{"schemaVersion":1,"connection":{{"type":"ssh","target":{{"host":"vm.test","username":"dev"}},"projectRoot":"{}"}},"env":{{"CI":"1"}}}}"#,
        f.dir.join("vm").display()
    );
    let write = |name: &str, body: &str| {
        let path = scripts.join(name);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$BRANCHYARD_RECIPE_MODE $BRANCHYARD_RECIPE_INSTANCE\" >>'{}'\n{body}",
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    };
    let print = format!(
        "mkdir -p '{}'\necho '{result}'\n",
        f.dir.join("vm").display()
    );
    RecipeOptions {
        name: "devbox".into(),
        create: write("create.sh", &print),
        suspend: Some(write("suspend.sh", "cat >/dev/null\n")),
        resume: Some(write("resume.sh", &format!("cat >/dev/null\n{print}"))),
        destroy: Some(write("destroy.sh", "cat >/dev/null\n")),
        workdir: f.dir.join("vm/workspace").display().to_string(),
        home: f.dir.join("vm/home").display().to_string(),
        pass_env: vec!["BY_TEST_VISIBLE".into()],
        ..RecipeOptions::default()
    }
}

fn options(f: &Fixture, recipe: &RecipeOptions) -> TaskOptions {
    TaskOptions {
        provider: Some(Provider::Recipe(recipe.clone())),
        ..f.options()
    }
}

#[test]
fn captured_ignored_outputs_return_over_the_copied_provider_without_a_git_candidate() {
    let f = Fixture::new();
    let recipe = recipe(&f);
    fs::write(f.root.join(".gitignore"), "outputs/\n").unwrap();
    f.git(&["add", ".gitignore"]);
    f.git(&["commit", "-qm", "Ignore generated outbox"]);
    let mut result = branchyard::ResultContract::deliverable(serde_json::json!({
        "type":"object", "required":["answer"], "additionalProperties":false,
        "properties":{"answer":{"type":"string"}}
    }))
    .unwrap();
    result.capture = Some(branchyard::ResultCapture::new("outputs/artifacts.json"));
    let digest = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b"copied evidence"));
    let manifest = serde_json::json!({"schema_version":1,"files":[{
        "path":"nested/evidence.txt","media_type":"text/plain","sha256":digest
    }]})
    .to_string();
    let prompt = format!(
        "SH mkdir -p outputs/nested && printf %s 'copied evidence' > outputs/nested/evidence.txt && printf %s unrelated > outputs/private.txt && printf %s {} > outputs/artifacts.json && printf '\\n```json\\n{{\"answer\":\"checked\"}}\\n```\\n'",
        branchyard_recipe::quote(&manifest)
    );
    let branch = f
        .yard
        .task(prompt)
        .options(TaskOptions {
            result: Some(result),
            ..options(&f, &recipe)
        })
        .name("capture")
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::NoChanges);
    assert!(branch.info().candidate.is_none());
    assert!(!branch.info().worktree.join("outputs").exists());
    let artifacts = f.yard.artifacts("capture").unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].labels["capture.path"], "nested/evidence.txt");
    let target = f.dir.join("captured.txt");
    f.yard
        .read_artifact("capture", &artifacts[0].id, &target)
        .unwrap();
    assert_eq!(fs::read(target).unwrap(), b"copied evidence");
    assert_eq!(
        branchyard::contract::current(&branch.events().unwrap()),
        Some(serde_json::json!({"answer":"checked"}))
    );
    assert_eq!(
        fs::read_dir(f.root.join(".branchyard/capture-staging"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn copied_capture_limits_digest_and_symlinks_fail_without_partial_snapshots_or_results() {
    let f = Fixture::new();
    let recipe = recipe(&f);
    fs::write(f.root.join(".gitignore"), "outputs/\n").unwrap();
    f.git(&["add", ".gitignore"]);
    f.git(&["commit", "-qm", "Ignore generated outbox"]);
    for (name, limit, digest, setup) in [
        (
            "size",
            3,
            hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b"copied evidence")),
            "printf %s 'copied evidence' > outputs/evidence.txt",
        ),
        (
            "digest",
            64,
            "0".repeat(64),
            "printf %s 'copied evidence' > outputs/evidence.txt",
        ),
        (
            "symlink",
            64,
            "0".repeat(64),
            "ln -s /etc/hosts outputs/evidence.txt",
        ),
        ("count", 64, "0".repeat(64), "true"),
    ] {
        let mut result =
            branchyard::ResultContract::deliverable(serde_json::json!({"type":"object"})).unwrap();
        let mut capture = branchyard::ResultCapture::new("outputs/artifacts.json");
        capture.max_bytes = limit;
        capture.max_files = 1;
        result.capture = Some(capture);
        let mut manifest = serde_json::json!({"schema_version":1,"files":[{
            "path":"evidence.txt","media_type":"text/plain","sha256":digest
        }]});
        if name == "count" {
            let mut second = manifest["files"][0].clone();
            second["path"] = serde_json::json!("other.txt");
            manifest["files"].as_array_mut().unwrap().push(second);
        }
        let manifest = manifest.to_string();
        let prompt = format!("SH mkdir -p outputs && {setup} && printf %s {} > outputs/artifacts.json && printf '\\n```json\\n{{}}\\n```\\n'", branchyard_recipe::quote(&manifest));
        assert!(f
            .yard
            .task(prompt)
            .options(TaskOptions {
                result: Some(result),
                ..options(&f, &recipe)
            })
            .name(name)
            .run()
            .is_err());
        let branch = f.yard.branch(name).unwrap();
        assert!(matches!(branch.info().status, BranchStatus::Failed { .. }));
        assert!(f.yard.artifacts(name).unwrap().is_empty());
        assert_eq!(
            branchyard::contract::current(&branch.events().unwrap()),
            None
        );
        assert_eq!(
            fs::read_dir(f.root.join(".branchyard/capture-staging"))
                .unwrap()
                .count(),
            0
        );
    }
}

#[test]
fn copied_capture_refuses_an_image_without_python_before_the_harness_turn() {
    let f = Fixture::new();
    let recipe = recipe(&f);
    let script = fs::read_to_string(&recipe.create).unwrap();
    fs::write(
        &recipe.create,
        script.replace("\"CI\":\"1\"", "\"PATH\":\"/no-such-image-tools\""),
    )
    .unwrap();
    let mut result =
        branchyard::ResultContract::deliverable(serde_json::json!({"type":"object"})).unwrap();
    result.capture = Some(branchyard::ResultCapture::new("outputs/artifacts.json"));
    let branch = f
        .yard
        .task("No model must start")
        .options(TaskOptions {
            result: Some(result),
            ..options(&f, &recipe)
        })
        .name("no-python")
        .run()
        .unwrap();
    let BranchStatus::Failed { ref reason } = branch.info().status else {
        panic!("missing prerequisite must fail before a turn")
    };
    assert!(reason.contains("requires Python3"), "{reason}");
    assert_eq!(branch.info().turns, 0);
}

#[test]
fn a_copied_managed_lead_defers_its_missing_outbox_until_the_final_wake() {
    let f = Fixture::new();
    let mut recipe = recipe(&f);
    // Default guest paths are distinct for each branch, as real machines are.
    recipe.workdir.clear();
    recipe.home.clear();
    fs::write(f.root.join(".gitignore"), "outputs/\n").unwrap();
    f.git(&["add", ".gitignore"]);
    f.git(&["commit", "-qm", "Ignore generated outbox"]);
    let digest = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b"final evidence"));
    let manifest = serde_json::json!({"schema_version":1,"files":[{
        "path":"evidence.txt","media_type":"text/plain","sha256":digest
    }]})
    .to_string();
    let wake = f.dir.join("wake-capture.txt");
    fs::write(&wake, format!("SH mkdir -p outputs && printf %s 'final evidence' > outputs/evidence.txt && printf %s {} > outputs/artifacts.json && printf '\\n```json\\n{{}}\\n```\\n'", branchyard_recipe::quote(&manifest))).unwrap();
    let mut result =
        branchyard::ResultContract::deliverable(serde_json::json!({"type":"object"})).unwrap();
    result.capture = Some(branchyard::ResultCapture::new("outputs/artifacts.json"));
    let agent = fake_agent().display().to_string();
    let options = TaskOptions {
        command: Some(vec![
            "env".into(),
            format!("FAKE_ACP_WAKE={}", wake.display()),
            agent.clone(),
        ]),
        delegation: Some(branchyard::Envelope::default()),
        delegation_server: Some(vec![agent]),
        policy: Policy::allow_all(),
        result: Some(result),
        ..options(&f, &recipe)
    };
    let yard = f.yard.clone();
    let lead_options = options.clone();
    let running = std::thread::spawn(move || {
        yard.task("AWAIT_STEER")
            .options(lead_options)
            .name("lead")
            .run()
    });
    wait::until("the copied lead to start its own work", || {
        f.yard
            .branch("lead")
            .ok()
            .filter(|branch| text(&branch.events().unwrap()).contains("waiting for steering"))
    });
    let lead = f.yard.branch("lead").unwrap();
    let delegate = lead.delegate(options).unwrap();
    delegate
        .spawn(branchyard::Spawn {
            prompt: "AWAIT_STEER".into(),
            name: Some("child".into()),
            ..branchyard::Spawn::default()
        })
        .unwrap();
    wait::until("the copied child to remain active", || {
        delegate
            .inspect("child")
            .ok()
            .filter(|child| child.last_message.contains("waiting for steering"))
    });
    lead.steer("Wait for your child").unwrap();
    wait::until(
        "the copied lead to park without an intermediate outbox",
        || {
            f.yard
                .branch("lead")
                .ok()
                .filter(|branch| branch.info().status == BranchStatus::WaitingOnChildren)
        },
    );
    assert!(f.yard.artifacts("lead").unwrap().is_empty());
    assert_eq!(
        branchyard::contract::current(&f.yard.branch("lead").unwrap().events().unwrap()),
        None
    );
    delegate.steer("child", "Done").unwrap();
    wait::until(
        "the copied lead's final wake to capture its ignored files",
        || {
            f.yard.branch("lead").ok().filter(|branch| {
                matches!(
                    branch.info().status,
                    BranchStatus::Ready | BranchStatus::NoChanges
                ) && branchyard::contract::current(&branch.events().unwrap())
                    == Some(serde_json::json!({}))
                    && f.yard.artifacts("lead").unwrap().len() == 1
            })
        },
    );
    running.join().unwrap().unwrap();
    for name in ["lead", "child"] {
        let branch = f.yard.branch(name).unwrap();
        let guest = recipe.workdir(name, &branch.info().worktree);
        branchyard_support::cleanup_dir(Path::new(&guest).parent().unwrap());
    }
    assert_eq!(
        fs::read_dir(f.root.join(".branchyard/capture-staging"))
            .unwrap()
            .count(),
        0
    );
}

/// What the recipe's scripts ran, in order: `mode instance` lines.
fn log(f: &Fixture) -> Vec<String> {
    fs::read_to_string(f.dir.join("recipe.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The machines the store still records.
fn recorded(f: &Fixture) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(f.root.join(".branchyard/recipes"))
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn sandbox_events(branch: &branchyard::Branch) -> Vec<SandboxEvent> {
    branch
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::Sandbox(event) => Some(*event),
            _ => None,
        })
        .collect()
}

#[test]
fn a_turn_on_a_recipe_machine_produces_a_candidate_that_merges() {
    let f = Fixture::new();
    let recipe = recipe(&f);
    fs::write(f.root.join("dirty.txt"), "uncommitted on main\n").unwrap();
    let branch = f
        .task("WRITE hello.txt=from-the-machine WRITE a.txt=rewritten")
        .options(options(&f, &recipe))
        .name("on-vm")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let info = branch.info();
    assert_eq!(info.status, BranchStatus::Ready, "{:?}", branch.events());
    let candidate = info.candidate.as_ref().unwrap();
    assert_eq!(
        git(
            &f.root,
            &["show", &format!("{}:hello.txt", candidate.commit)]
        ),
        "from-the-machine\n"
    );
    // The harness worked on the machine's copy, which was made through
    // the recipe, and the machine was destroyed when the turn ended.
    let machine = match &sandbox_events(&branch)[0] {
        SandboxEvent::Started {
            provider,
            sandbox,
            placed: Some(placed),
            ..
        } => {
            assert_eq!(provider, "recipe");
            assert!(placed.worktree_push.bytes > 0, "{placed:?}");
            sandbox.clone()
        }
        other => panic!("{other:?}"),
    };
    assert!(
        sandbox_events(&branch)
            .iter()
            .any(|e| matches!(e, SandboxEvent::Returned { sandbox, .. } if *sandbox == machine)),
        "{:?}",
        sandbox_events(&branch)
    );
    assert_eq!(
        log(&f),
        [format!("create {machine}"), format!("destroy {machine}")]
    );
    assert!(recorded(&f).is_empty(), "{:?}", recorded(&f));
    assert!(f.dir.join("vm/workspace/hello.txt").is_file());

    let merged = f.yard.merge("on-vm", "main").unwrap();
    assert_eq!(
        git(&f.root, &["show", &format!("{}:hello.txt", merged.commit)]),
        "from-the-machine\n"
    );
    assert_eq!(git(&f.root, &["show", "main:a.txt"]), "rewritten\n");
}

/// A preset's file guard is for a harness on this host: a machine is its
/// own boundary, and the branch records that it got no guard and why.
#[test]
fn a_recipe_machine_gets_no_file_guard_and_the_record_says_why() {
    let f = Fixture::new();
    let recipe = recipe(&f);
    let branch = f
        .task("WRITE hello.txt=from-the-machine")
        .options(options(&f, &recipe))
        .name("unguarded")
        .policy(branchyard::PolicyPreset::EditWorktree.policy())
        .run()
        .unwrap();
    let info = branch.info();
    assert_eq!(info.status, BranchStatus::Ready, "{:?}", branch.events());
    assert_eq!(
        info.guard,
        Some(branchyard::Guard::Unguarded {
            reason: "the harness runs in a sandbox (recipe:devbox), which is its boundary".into()
        })
    );
}

#[test]
fn the_harness_gets_the_machine_home_the_result_env_and_passed_variables() {
    let f = Fixture::new();
    let recipe = recipe(&f);
    let branch = f
        .task("ENV CI BY_TEST_VISIBLE")
        .options(options(&f, &recipe))
        .name("env-on-vm")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::NoChanges);
    let said = text(&branch.events().unwrap());
    assert!(
        said.contains(&format!("HOME={}", f.dir.join("vm/home").display())),
        "{said}"
    );
    assert!(said.contains("CI=1"), "{said}");
    assert!(said.contains("BY_TEST_VISIBLE=yes"), "{said}");
    // Anything else is the machine's own login environment, which the fake
    // ssh takes from this process, as an sshd would from the login.
}

#[test]
fn a_provisioned_home_goes_to_the_machine_and_comes_back_private() {
    let f = Fixture::new();
    let recipe = recipe(&f);
    let secret = "sk-proj-recipe-SECRET-0123456789";
    std::env::set_var("BY_TEST_RECIPE_OPENAI", secret);
    let branch = f
        .task(
            "SH stat -c '%a' \"$HOME/.codex/auth.json\"\n\
             SH cat \"$HOME/.codex/config.toml\"\n\
             SH echo \"codex-home=$CODEX_HOME\"",
        )
        .options(TaskOptions {
            harness: Some("codex-acp".into()),
            provision: Some(Provisioning {
                secrets: vec![SecretSource::parse("OPENAI_API_KEY=BY_TEST_RECIPE_OPENAI").unwrap()],
                effort: Some(Effort::Low),
                ..Provisioning::default()
            }),
            ..options(&f, &recipe)
        })
        .name("provisioned-vm")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    let said = text(&events);
    assert!(said.contains("600"), "{said}");
    assert!(said.contains("model_reasoning_effort = \"low\""), "{said}");
    assert!(
        said.contains(&format!("codex-home={}/.codex", recipe.home)),
        "{said}"
    );
    assert!(!serde_json::to_string(&events).unwrap().contains(secret));
    // The home came back with the harness's files, still private.
    let home = std::path::PathBuf::from(
        stored_record(&f.root, "provisioned-vm")["home"]
            .as_str()
            .unwrap(),
    );
    let auth = home.join(".codex/auth.json");
    assert!(fs::read_to_string(&auth).unwrap().contains(secret));
    let mode = fs::metadata(&auth).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn a_kept_machine_is_suspended_resumed_and_destroyed_with_the_branch() {
    let f = Fixture::new();
    let mut recipe = recipe(&f);
    recipe.keep = SandboxKeep::Pause;
    let kept = options(&f, &recipe);
    let branch = f
        .task("WRITE one.txt=1")
        .options(kept.clone())
        .name("kept-vm")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    assert_eq!(
        branch.info().status,
        BranchStatus::Ready,
        "{:?}",
        branch.events()
    );
    let events = sandbox_events(&branch);
    let machine = match &events[0] {
        SandboxEvent::Started { sandbox, .. } => sandbox.clone(),
        other => panic!("{other:?}"),
    };
    assert!(
        matches!(&events[1], SandboxEvent::Returned { .. }),
        "the home comes back before the machine is suspended: {events:?}"
    );
    assert!(
        matches!(&events[2], SandboxEvent::Kept { .. }),
        "{events:?}"
    );
    assert_eq!(
        log(&f),
        [format!("create {machine}"), format!("suspend {machine}")]
    );
    assert_eq!(recorded(&f).len(), 1);

    // The next turn resumes the same machine, with the worktree sent again.
    let sent = branch.send("WRITE two.txt=2", kept.clone()).unwrap();
    assert_eq!(
        sent.info().status,
        BranchStatus::Ready,
        "{:?}",
        sent.events()
    );
    let resumed = sandbox_events(&sent)
        .into_iter()
        .filter_map(|e| match e {
            SandboxEvent::Started {
                sandbox, origin, ..
            } => Some((sandbox, origin)),
            _ => None,
        })
        .nth(1)
        .unwrap();
    assert_eq!(resumed, (machine.clone(), SandboxOrigin::Resumed));
    let candidate = sent.info().candidate.clone().unwrap().commit;
    for file in ["one.txt", "two.txt"] {
        git(&f.root, &["cat-file", "-e", &format!("{candidate}:{file}")]);
    }
    assert_eq!(
        log(&f)[2..],
        [format!("resume {machine}"), format!("suspend {machine}")]
    );

    // Removing the branch destroys the kept machine through the recipe.
    f.yard.remove("kept-vm").unwrap();
    assert_eq!(log(&f).last(), Some(&format!("destroy {machine}")));
    assert!(recorded(&f).is_empty(), "{:?}", recorded(&f));
}

#[test]
fn a_recipe_without_suspend_cannot_keep_its_machine() {
    let f = Fixture::new();
    let mut recipe = recipe(&f);
    recipe.keep = SandboxKeep::Pause;
    recipe.suspend = None;
    let Err(error) = f
        .task("WRITE x=1")
        .options(options(&f, &recipe))
        .name("cannot-keep")
        .run()
    else {
        panic!("a recipe without suspend kept its machine")
    };
    assert!(
        error.to_string().contains("needs both suspend and resume"),
        "{error}"
    );
    assert!(log(&f).is_empty(), "nothing ran");
}

/// Run a turn with the provider in `BY_CHILD_PROVIDER` on a branch named
/// `crashy`. Run only as the child of the recovery test, which kills it.
#[test]
#[ignore = "the child process of the recovery test"]
fn recipe_engine_child() {
    let (Some(root), Some(agent), Some(provider)) = (
        std::env::var_os("BY_CHILD_ROOT"),
        std::env::var("BY_CHILD_AGENT").ok(),
        std::env::var("BY_CHILD_PROVIDER").ok(),
    ) else {
        return;
    };
    let provider: Provider = serde_json::from_str(&provider).unwrap();
    let yard = Yard::open(root).unwrap();
    yard.task("ORPHAN")
        .harness("gemini-cli")
        .command([agent])
        .provider(provider)
        .policy(Policy::allow_all())
        .name("crashy")
        .run()
        .unwrap();
}

#[test]
fn recovery_brings_back_the_work_of_an_engine_that_died_and_destroys_its_machine() {
    let f = Fixture::new();
    let recipe = recipe(&f);
    let provider = Provider::Recipe(recipe.clone());
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["recipe_engine_child", "--exact", "--ignored"])
        .env("BY_CHILD_ROOT", &f.root)
        .env("BY_CHILD_AGENT", fake_agent())
        .env(
            "BY_CHILD_PROVIDER",
            serde_json::to_string(&provider).unwrap(),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The ORPHAN prompt writes orphan.log on the machine and hangs.
    let pids: Vec<u32> = wait::until("the harness to start", || {
        let said = f
            .yard
            .branch("crashy")
            .ok()
            .and_then(|b| b.events().ok())
            .map(|e| text(&e))
            .unwrap_or_default();
        said.strip_prefix("orphan ").map(|rest| {
            rest.split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect()
        })
    });
    let machine = recorded(&f)
        .pop()
        .expect("the turn's machine")
        .trim_end_matches(".json")
        .to_owned();
    assert!(f.root.join(".branchyard/transfer").join(&machine).is_dir());
    // The machine is in the repository's service registry, owned by the
    // engine that made it, with what reclaims it.
    let registry =
        branchyard::services::LocalRegistry::open(f.root.join(".branchyard/registry.db")).unwrap();
    let listed = branchyard::services::ServiceStore::all(&registry)
        .unwrap()
        .into_iter()
        .find(|s| s.kind == branchyard::services::KIND_RECIPE_MACHINE)
        .expect("the machine, registered");
    assert_eq!(listed.text("sandbox"), Some(machine.as_str()));
    assert_eq!(listed.owner.pid, child.id());
    assert_eq!(listed.owner.branch.as_deref(), Some("crashy"));
    assert_eq!(listed.state, branchyard::services::ServiceState::Live);
    child.kill().unwrap();
    child.wait().unwrap();
    // Unlike a bridge, ssh leaves the harness running on the machine when
    // the engine's connection closes.
    assert!(pids.iter().all(|pid| wait::alive(*pid)), "{pids:?}");
    let worktree = f.root.join(".branchyard/worktrees/crashy");
    assert!(!worktree.join("orphan.log").exists(), "still only there");

    let yard = Yard::open(&f.root).unwrap();
    let branch = yard.branch("crashy").unwrap();
    assert_eq!(branch.info().status, BranchStatus::Interrupted);
    let reasons: Vec<String> = branch
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::Recovered { reason, .. } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(
        reasons[0].contains(&format!(
            "brought the harness's work in machine {machine} back to the worktree; destroyed \
             its machine {machine} (recipe devbox)"
        )),
        "{}",
        reasons[0]
    );
    for pid in &pids {
        wait::gone(*pid);
    }
    assert_eq!(
        fs::read_to_string(worktree.join("orphan.log")).unwrap(),
        "prompt received\n"
    );
    let candidate = branch.info().candidate.clone().expect("a candidate");
    assert_eq!(
        git(
            &f.root,
            &["show", &format!("{}:orphan.log", candidate.commit)]
        ),
        "prompt received\n"
    );
    assert_eq!(log(&f).last(), Some(&format!("destroy {machine}")));
    assert!(recorded(&f).is_empty());
    assert!(!f.root.join(".branchyard/transfer").join(&machine).exists());
    assert!(yard.recover().unwrap().is_empty());
    // Its record was reaped when the yard opened: recovery had destroyed
    // the machine already, so the reaper found it gone.
    let row = branchyard::services::ServiceStore::get(&registry, &listed.id)
        .unwrap()
        .unwrap();
    assert_eq!(row.state, branchyard::services::ServiceState::Reclaimed);
    assert_eq!(
        row.note.as_deref(),
        Some(format!("machine {machine} was already gone").as_str())
    );
}
