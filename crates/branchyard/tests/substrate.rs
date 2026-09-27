//! Turns in Agent Substrate actors, end to end against the fake cluster in
//! `branchyard_substrate::fake`: the fake ACP agent runs behind a real
//! bridge, the worktree and home go in and come back, the candidate merges,
//! and recovery deletes the actor of an engine that died. Hermetic; not
//! evidence about a Substrate cluster.

mod common;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use branchyard::{
    Activity, BranchStatus, Envelope, Policy, Provider, SubstrateOptions, TaskOptions, Yard,
};
use branchyard_bridge::Signer;
use branchyard_substrate::fake::FakeCluster;
use branchyard_substrate::pb;
use branchyard_substrate::template::{bridge_template, BridgeTemplate};
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
    wait_gone(pid);
    assert!(fake.actor_names().is_empty());
    assert_eq!(sent.info().turns, 4);
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

fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        let state = stat
            .rsplit(')')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .next();
        !matches!(state, Some("Z") | Some("X"))
    })
}

fn wait_gone(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while alive(pid) {
        assert!(Instant::now() < deadline, "pid {pid} is still running");
        std::thread::sleep(Duration::from_millis(20));
    }
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

#[test]
fn recovery_deletes_the_actor_of_an_engine_that_died() {
    let f = Fixture::new();
    let (fake, substrate) = cluster(&f);
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
        wait_gone(*pid);
    }
    assert_eq!(fake.actor_names(), vec![actor.clone()]);

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
        reasons[0].contains(&format!("deleted its Substrate actor {actor}")),
        "{}",
        reasons[0]
    );
    assert!(fake.actor_names().is_empty());
    wait_gone(bridge);
    assert!(
        !f.root.join(".branchyard/transfer").join(&actor).exists(),
        "the dead engine's transfer staging was left behind"
    );
}
