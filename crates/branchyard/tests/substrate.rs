//! Turns in Agent Substrate actors, end to end against the fake cluster in
//! `branchyard_substrate::fake`: the fake ACP agent runs behind a real
//! bridge, the worktree and home go in and come back, the candidate merges,
//! and recovery brings back the work of an engine that died, unless the
//! worktree changed since, and deletes its actor. Hermetic; not evidence
//! about a Substrate cluster.

mod common;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use branchyard::{
    Activity, BranchStatus, Effort, Envelope, Policy, Provider, Provisioning, SecretSource,
    SubstrateOptions, TaskOptions, Yard,
};
use branchyard_bridge::Signer;
use branchyard_substrate::fake::FakeCluster;
use branchyard_substrate::pb;
use branchyard_substrate::template::{bridge_template, BridgeTemplate};
use branchyard_testkit::wait;
use common::{bridge_binary, fake_agent, git, stored_record, text, Fixture};

const ATESPACE: &str = "yard";
const TEMPLATE: &str = "by-harness";

/// A fake cluster for the fixture, and the provider options that reach it.
/// The actor's workdir and home are directories on this host, emptied
/// whenever an actor is created, as a fresh root filesystem would be.
fn cluster(f: &Fixture) -> (FakeCluster, SubstrateOptions) {
    let key = f.dir.join("bridge.key");
    let signer = Signer::write(&key).unwrap();
    let fake = FakeCluster::start(
        ATESPACE,
        Some(bridge_binary().to_path_buf()),
        &f.dir.join("cluster"),
    );
    fake.add_template(bridge_template(
        ATESPACE,
        &BridgeTemplate {
            name: TEMPLATE.into(),
            image: "registry.example/by@sha256:00".into(),
            bridge: "/usr/local/bin/branchyard-bridge".into(),
            public_key: signer.public_key(),
            sandbox_class: pb::SandboxClass::Gvisor,
            sandbox_config: "gvisor".into(),
            storage_location: "gs://bucket/by".into(),
        },
    ));
    let workdir = f.dir.join("actor/workspace");
    let home = f.dir.join("actor/home");
    fake.fresh_on_create(&workdir);
    fake.fresh_on_create(&home);
    let options = SubstrateOptions {
        endpoint: fake.endpoint().into(),
        router: fake.router().into(),
        atespace: ATESPACE.into(),
        template: TEMPLATE.into(),
        key,
        workdir: workdir.display().to_string(),
        home: home.display().to_string(),
        pass_env: vec!["BY_TEST_VISIBLE".into()],
        ..SubstrateOptions::default()
    };
    (fake, options)
}

fn options(f: &Fixture, substrate: &SubstrateOptions) -> TaskOptions {
    TaskOptions {
        provider: Some(Provider::Substrate(substrate.clone())),
        ..f.options()
    }
}

#[test]
fn a_turn_in_an_actor_produces_a_candidate_that_merges() {
    let f = Fixture::new();
    let (fake, substrate) = cluster(&f);
    std::fs::write(f.root.join("dirty.txt"), "uncommitted on main\n").unwrap();
    let branch = f
        .task("WRITE hello.txt=from-the-actor WRITE a.txt=rewritten")
        .options(options(&f, &substrate))
        .name("remote")
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
        "from-the-actor\n"
    );
    // The harness worked on the actor's copy, never on this host's files.
    let said = text(&branch.events().unwrap());
    assert!(said.contains("wrote hello.txt"), "{said}");
    assert!(
        fake.actor_names().is_empty(),
        "the turn's actor was deleted"
    );

    let merged = f.yard.merge("remote", "main").unwrap();
    assert_eq!(
        git(&f.root, &["show", &format!("{}:hello.txt", merged.commit)]),
        "from-the-actor\n"
    );
    assert_eq!(git(&f.root, &["show", "main:a.txt"]), "rewritten\n");
}

#[test]
fn commits_made_in_an_actor_reach_the_branch_as_commits() {
    let f = Fixture::new();
    let (fake, substrate) = cluster(&f);
    let base = git(&f.root, &["rev-parse", "HEAD"]).trim().to_owned();
    let branch = f
        .task(
            "SH printf one > one.txt && git add one.txt && git commit -q -m 'First in the actor'\n\
             SH printf two > two.txt && git add two.txt && git commit -q -m 'Second in the actor'\n\
             SH printf dirty > dirty-in-actor.txt",
        )
        .options(options(&f, &substrate))
        .name("committer")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let info = branch.info();
    assert_eq!(info.status, BranchStatus::Ready, "{:?}", branch.events());
    let candidate = info.candidate.as_ref().unwrap();
    // The harness's commits, in order, then the engine's snapshot of what
    // it left uncommitted.
    let log = git(
        &f.root,
        &[
            "log",
            "--format=%s",
            &format!("{base}..{}", candidate.commit),
        ],
    );
    let subjects: Vec<&str> = log.lines().collect();
    assert_eq!(subjects.len(), 3, "{log}");
    assert_eq!(subjects[1..], ["Second in the actor", "First in the actor"]);
    assert_eq!(
        git(
            &f.root,
            &["show", &format!("{}:dirty-in-actor.txt", candidate.commit)]
        ),
        "dirty"
    );
    assert!(fake.actor_names().is_empty());
    let merged = f.yard.merge("committer", "main").unwrap();
    let log = git(&f.root, &["log", "--format=%s", &merged.commit]);
    assert!(log.contains("First in the actor"), "{log}");
}

#[test]
fn the_home_and_session_carry_over_between_turns_and_nothing_stays_running() {
    let f = Fixture::new();
    let (fake, substrate) = cluster(&f);
    let branch = f
        .task("ENV BY_TEST_VISIBLE CLAUDECODE")
        .options(options(&f, &substrate))
        .name("homed")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let said = text(&branch.events().unwrap());
    assert!(said.contains(&format!("HOME={}", substrate.home)), "{said}");
    assert!(said.contains("BY_TEST_VISIBLE=yes"), "{said}");
    assert!(fake.actor_names().is_empty());

    // What the harness leaves in its home comes back to the branch's
    // private home, and goes into the next turn's actor.
    let sent = branch
        .send("SH printf kept > \"$HOME/marker\"", options(&f, &substrate))
        .unwrap();
    let home = PathBuf::from(stored_record(&f.root, "homed")["home"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(home.join("marker")).unwrap(),
        "kept",
        "{}",
        text(&sent.events().unwrap())
    );
    let sent = branch
        .send("SH cat \"$HOME/marker\"", options(&f, &substrate))
        .unwrap();
    let said = text(&sent.events().unwrap());
    assert!(said.contains("kept"), "{said}");

    // A process the harness leaves behind ends with the turn's actor.
    let sent = branch.send("BACKGROUND", options(&f, &substrate)).unwrap();
    let said = text(&sent.events().unwrap());
    let pid: u32 = said
        .split("background pid ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
        .unwrap_or_else(|| panic!("no background pid in {said:?}"));
    wait::gone(pid);
    assert!(fake.actor_names().is_empty());
    assert_eq!(sent.info().turns, 4);
}

#[test]
fn a_provisioned_home_goes_into_the_actor_and_comes_back() {
    let f = Fixture::new();
    let (fake, substrate) = cluster(&f);
    let secret = "sk-proj-substrate-SECRET-0123456789";
    std::env::set_var("BY_TEST_SUBSTRATE_OPENAI", secret);
    let branch = f
        .task(
            "SH stat -c '%a' \"$HOME/.codex/auth.json\"\n\
             SH cat \"$HOME/.codex/config.toml\"\n\
             SH echo \"codex-home=$CODEX_HOME\"",
        )
        .options(TaskOptions {
            harness: Some("codex-acp".into()),
            provision: Some(Provisioning {
                secrets: vec![
                    SecretSource::parse("OPENAI_API_KEY=BY_TEST_SUBSTRATE_OPENAI").unwrap(),
                ],
                effort: Some(Effort::Low),
                ..Provisioning::default()
            }),
            ..options(&f, &substrate)
        })
        .name("provisioned-actor")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    let said = text(&events);
    assert!(said.contains("600"), "{said}");
    assert!(said.contains("model_reasoning_effort = \"low\""), "{said}");
    assert!(
        said.contains(&format!("codex-home={}/.codex", substrate.home)),
        "{said}"
    );
    assert!(!serde_json::to_string(&events).unwrap().contains(secret));
    assert!(fake.actor_names().is_empty());
    // The home came back with the harness's files, still private.
    let home = PathBuf::from(
        stored_record(&f.root, "provisioned-actor")["home"]
            .as_str()
            .unwrap(),
    );
    let auth = home.join(".codex/auth.json");
    assert!(std::fs::read_to_string(&auth).unwrap().contains(secret));
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&auth).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn a_local_branch_forks_into_an_actor_with_its_secrets() {
    let f = Fixture::new();
    let (fake, substrate) = cluster(&f);
    // A branch that ran with your own home, then a fork into an actor,
    // which gets a private home of its own for the secret.
    let parent = f.task("WRITE a.txt=1").name("local").run().unwrap();
    std::env::set_var(
        "BY_TEST_SUBSTRATE_FORK_OPENAI",
        "sk-proj-fork-SECRET-0123456789",
    );
    let fork = parent
        .fork(
            "SH stat -c '%a' \"$HOME/.codex/auth.json\"",
            true,
            TaskOptions {
                harness: Some("codex-acp".into()),
                name: Some("in-actor".into()),
                provision: Some(Provisioning {
                    secrets: vec![SecretSource::parse(
                        "OPENAI_API_KEY=BY_TEST_SUBSTRATE_FORK_OPENAI",
                    )
                    .unwrap()],
                    ..Provisioning::default()
                }),
                policy: Policy::allow_all(),
                ..options(&f, &substrate)
            },
        )
        .unwrap();
    assert!(text(&fork.events().unwrap()).contains("600"));
    assert!(fake.actor_names().is_empty());
}

#[test]
fn delegation_is_refused_to_a_harness_in_an_actor() {
    let f = Fixture::new();
    let (fake, substrate) = cluster(&f);
    let branch = f
        .task("WRITE x.txt=1")
        .options(TaskOptions {
            delegation: Some(Envelope::default()),
            delegation_server: Some(vec![fake_agent().display().to_string()]),
            ..options(&f, &substrate)
        })
        .name("delegating")
        .run()
        .unwrap();
    assert!(
        matches!(&branch.info().status, BranchStatus::Failed { reason } if reason.contains("sandboxed")),
        "{:?}",
        branch.info().status
    );
    assert!(fake.actor_names().is_empty());
}

/// Run a turn with the provider in `BY_CHILD_PROVIDER` on a branch named
/// `crashy`. Run only as the child of the recovery test, which kills it.
#[test]
#[ignore = "the child process of the recovery test"]
fn substrate_engine_child() {
    let (Some(root), Some(prompt), Some(agent), Some(provider)) = (
        std::env::var_os("BY_CHILD_ROOT"),
        std::env::var("BY_CHILD_PROMPT").ok(),
        std::env::var("BY_CHILD_AGENT").ok(),
        std::env::var("BY_CHILD_PROVIDER").ok(),
    ) else {
        return;
    };
    let provider: Provider = serde_json::from_str(&provider).unwrap();
    let yard = Yard::open(root).unwrap();
    let _ = yard
        .task(prompt)
        .harness("gemini-cli")
        .command([agent])
        .provider(provider)
        .policy(Policy::allow_all())
        .name("crashy")
        .run();
}

/// Start a turn in an actor in another process with the ORPHAN prompt,
/// which writes `orphan.log` in the actor's worktree and hangs, kill that
/// engine, and wait for the bridge to end the harness. Returns the actor's
/// name and its bridge's pid.
fn kill_engine_mid_turn(
    f: &Fixture,
    fake: &FakeCluster,
    substrate: &SubstrateOptions,
) -> (String, u32) {
    let provider = Provider::Substrate(substrate.clone());
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["substrate_engine_child", "--exact", "--ignored"])
        .env("BY_CHILD_ROOT", &f.root)
        .env("BY_CHILD_AGENT", fake_agent())
        .env("BY_CHILD_PROMPT", "ORPHAN")
        .env(
            "BY_CHILD_PROVIDER",
            serde_json::to_string(&provider).unwrap(),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let pids: Vec<u32> = loop {
        assert!(Instant::now() < deadline, "the harness never started");
        let said = f
            .yard
            .branch("crashy")
            .ok()
            .and_then(|b| b.events().ok())
            .map(|e| text(&e))
            .unwrap_or_default();
        if let Some(rest) = said.strip_prefix("orphan ") {
            break rest
                .split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect();
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let actor = fake.actor_names().pop().expect("the turn's actor");
    let bridge = fake.bridge_pid(&actor).expect("its bridge");
    assert!(f.root.join(".branchyard/transfer").join(&actor).is_dir());

    child.kill().unwrap();
    child.wait().unwrap();
    // The dead engine's connection closed, so the bridge ended the harness;
    // the actor itself outlives the engine until recovery.
    for pid in &pids {
        wait::gone(*pid);
    }
    assert_eq!(fake.actor_names(), vec![actor.clone()]);
    (actor, bridge)
}

fn recovered_reason(yard: &Yard) -> String {
    let reasons: Vec<String> = yard
        .branch("crashy")
        .unwrap()
        .events()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.activity {
            Activity::Recovered { reason, .. } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    reasons[0].clone()
}

#[test]
fn recovery_brings_back_the_work_of_an_engine_that_died_and_deletes_its_actor() {
    let f = Fixture::new();
    let (fake, substrate) = cluster(&f);
    let (actor, bridge) = kill_engine_mid_turn(&f, &fake, &substrate);
    let worktree = f.root.join(".branchyard/worktrees/crashy");
    assert!(
        !worktree.join("orphan.log").exists(),
        "still only in the actor"
    );

    let yard = Yard::open(&f.root).unwrap();
    let branch = yard.branch("crashy").unwrap();
    assert_eq!(branch.info().status, BranchStatus::Interrupted);
    let reason = recovered_reason(&yard);
    assert!(reason.contains("the prompt had been submitted"), "{reason}");
    assert!(
        reason.contains(&format!(
            "brought the harness's work in actor {actor} back to the worktree; deleted its \
             Substrate actor {actor}"
        )),
        "{reason}"
    );
    // What the harness wrote in the actor is in the worktree and in the
    // candidate recovery snapshotted.
    assert_eq!(
        std::fs::read_to_string(worktree.join("orphan.log")).unwrap(),
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
    assert!(fake.actor_names().is_empty());
    wait::gone(bridge);
    assert!(
        !f.root.join(".branchyard/transfer").join(&actor).exists(),
        "the dead engine's transfer staging was left behind"
    );
    assert!(yard.recover().unwrap().is_empty());
}

#[test]
fn recovery_does_not_apply_an_actors_work_over_a_worktree_changed_since() {
    let f = Fixture::new();
    let (fake, substrate) = cluster(&f);
    let (actor, bridge) = kill_engine_mid_turn(&f, &fake, &substrate);
    let worktree = f.root.join(".branchyard/worktrees/crashy");
    std::fs::write(worktree.join("a.txt"), "changed on the host\n").unwrap();

    let yard = Yard::open(&f.root).unwrap();
    assert_eq!(
        yard.branch("crashy").unwrap().info().status,
        BranchStatus::Interrupted
    );
    let reason = recovered_reason(&yard);
    assert!(
        reason.contains(&format!(
            "did not bring the harness's work back from actor {actor}: the worktree changed on \
             the host"
        )),
        "{reason}"
    );
    assert!(
        reason.contains(&format!("deleted its Substrate actor {actor}")),
        "{reason}"
    );
    assert!(!worktree.join("orphan.log").exists());
    assert_eq!(
        std::fs::read_to_string(worktree.join("a.txt")).unwrap(),
        "changed on the host\n"
    );
    assert!(fake.actor_names().is_empty());
    wait::gone(bridge);
    assert!(!f.root.join(".branchyard/transfer").join(&actor).exists());
}

/// `keep = "pause"` on Substrate: the actor is paused (`PauseActor`) when a
/// turn ends and resumed (`ResumeActor`) by the next, with the worktree and
/// home sent again; each checkpoint suspends it and tags it (`CreateTag`);
/// a fork at a checkpoint creates its actor from the tag (`CreateActor`
/// with `source_tag`), created stopped and started, not a live fork: the
/// source actor is left suspended. Removal deletes the actor and its tags.
#[test]
fn a_kept_actor_is_paused_resumed_tagged_and_forked_from_its_tag() {
    use branchyard::{SandboxEvent, SandboxKeep, SandboxOrigin, SnapshotMethod};
    let f = Fixture::new();
    let (fake, mut substrate) = cluster(&f);
    substrate.keep = SandboxKeep::Pause;
    let kept = options(&f, &substrate);
    let branch = f
        .task("WRITE one.txt=1")
        .options(kept.clone())
        .name("kept-actor")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    assert_eq!(
        branch.info().status,
        BranchStatus::Ready,
        "{:?}",
        branch.events()
    );
    let sandbox_events = |branch: &branchyard::Branch| -> Vec<SandboxEvent> {
        branch
            .events()
            .unwrap()
            .into_iter()
            .filter_map(|e| match e.activity {
                Activity::Sandbox(event) => Some(*event),
                _ => None,
            })
            .collect()
    };
    let events = sandbox_events(&branch);
    let actor = match &events[0] {
        SandboxEvent::Started {
            sandbox, provider, ..
        } => {
            assert_eq!(provider, "substrate");
            sandbox.clone()
        }
        other => panic!("{other:?}"),
    };
    assert!(
        matches!(&events[1], SandboxEvent::Kept { .. }),
        "{events:?}"
    );
    assert_eq!(
        fake.actor_names(),
        std::slice::from_ref(&actor),
        "kept, not deleted"
    );
    // The checkpoint suspended it and tagged it.
    let checkpoint = branch.checkpoints().unwrap().checkpoints[0]
        .checkpoint
        .clone();
    let snapshot = checkpoint.sandbox.expect("a tag with checkpoint 1");
    assert_eq!(snapshot.method, SnapshotMethod::Checkpoint);
    assert_eq!(snapshot.provider, "substrate");
    assert!(fake.tag_names().contains(&snapshot.handle));
    let state = |name: &str| {
        fake.actor(name)
            .and_then(|a| a.status)
            .map(|s| s.state)
            .unwrap_or_default()
    };
    assert_eq!(state(&actor), pb::ActorState::Suspended as i32);

    // The next turn resumes the same actor, with the worktree sent again.
    let sent = branch.send("WRITE two.txt=2", kept.clone()).unwrap();
    assert_eq!(
        sent.info().status,
        BranchStatus::Ready,
        "{:?}",
        sent.events()
    );
    let events = sandbox_events(&sent);
    let resumed = events
        .iter()
        .filter_map(|e| match e {
            SandboxEvent::Started {
                sandbox, origin, ..
            } => Some((sandbox.clone(), origin.clone())),
            _ => None,
        })
        .nth(1)
        .unwrap();
    assert_eq!(resumed, (actor.clone(), SandboxOrigin::Resumed));
    let candidate = sent.info().candidate.clone().unwrap().commit;
    for file in ["one.txt", "two.txt"] {
        git(&f.root, &["cat-file", "-e", &format!("{candidate}:{file}")]);
    }

    // A fork at checkpoint 1: a new actor from its tag, not a live fork.
    let fork = sent
        .fork_at(
            1,
            "WRITE forked.txt=f",
            TaskOptions {
                name: Some("from-tag".into()),
                ..kept.clone()
            },
        )
        .unwrap();
    assert_eq!(
        fork.info().status,
        BranchStatus::Ready,
        "{:?}",
        fork.events()
    );
    let origin = sandbox_events(&fork)
        .into_iter()
        .find_map(|e| match e {
            SandboxEvent::Started { origin, .. } => Some(origin),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        origin,
        SandboxOrigin::Branched {
            branch: "kept-actor".into(),
            turn: 1,
            method: SnapshotMethod::Checkpoint,
        }
    );
    let fork_candidate = fork.info().candidate.clone().unwrap().commit;
    git(
        &f.root,
        &["cat-file", "-e", &format!("{fork_candidate}:one.txt")],
    );
    assert!(
        std::process::Command::new("git")
            .args(["cat-file", "-e", &format!("{fork_candidate}:two.txt")])
            .current_dir(&f.root)
            .status()
            .map(|s| !s.success())
            .unwrap_or(false),
        "the fork is at checkpoint 1"
    );
    assert_eq!(
        state(&actor),
        pb::ActorState::Suspended as i32,
        "the source actor was left suspended"
    );
    assert_eq!(fake.actor_names().len(), 2);

    // Removal deletes the actors and their tags.
    let tags_before = fake.tag_names().len();
    assert!(tags_before >= 2, "{:?}", fake.tag_names());
    f.yard.remove("from-tag").unwrap();
    f.yard.remove("kept-actor").unwrap();
    assert!(fake.actor_names().is_empty(), "{:?}", fake.actor_names());
    assert!(fake.tag_names().is_empty(), "{:?}", fake.tag_names());
}
