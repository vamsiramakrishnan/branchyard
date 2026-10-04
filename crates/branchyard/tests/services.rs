//! The service registry on one machine: the registry conformance on the
//! repository's SQLite file, registrations from several processes at
//! once, and reclamation: a killed owner's leaked process is stopped, a
//! process Branchyard did not start (a reused pid) is not, a recipe
//! machine is destroyed through its recorded destroy (reached through
//! `crates/branchyard-recipe/tests/fixtures/fake-ssh`), and pool slots are
//! reclaimed. Hermetic; owners are child processes of this test binary,
//! and nothing waits on time: leases are moved by an explicit clock, and
//! an owner killed on this host is known gone at once. Requires `git`,
//! `sh`, `sleep`, `ps` and `python3`.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use branchyard::services::{
    self, conformance, Endpoint, LocalRegistry, Outcome, Query, Reclaim, Service, ServiceOwner,
    ServiceState, ServiceStore,
};
use branchyard::{Provider, RecipeOptions, Yard};
use branchyard_sandbox::{SandboxProvider, SandboxSpec};
use branchyard_testkit::wait;
use common::Fixture;

const FAKE_SSH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../branchyard-recipe/tests/fixtures/fake-ssh"
);

/// A pid that no longer runs, with a start time no process has: an owner
/// known gone from this host.
fn gone_owner() -> ServiceOwner {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    let (host, _, _) = branchyard::process_identity();
    ServiceOwner {
        id: "o_gone".into(),
        host,
        pid,
        start: "0".into(),
        branch: None,
        operation: None,
        principal: None,
    }
}

#[test]
fn the_local_registry_conforms_in_a_file_and_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let file = LocalRegistry::open(dir.path().join("registry.db")).unwrap();
    conformance::check(&file, "sqlite file");
    conformance::check(&LocalRegistry::memory(), "sqlite memory");
}

#[test]
fn the_registry_file_is_private_and_made_private_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registry.db");
    LocalRegistry::open(&path).unwrap();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    LocalRegistry::open(&path).unwrap();
    assert_eq!(mode(&path), 0o600);
}

#[test]
fn handles_on_one_file_register_at_once_without_losing_any() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registry.db");
    let stores: Vec<Arc<dyn ServiceStore>> = (0..4)
        .map(|_| Arc::new(LocalRegistry::open(&path).unwrap()) as Arc<dyn ServiceStore>)
        .collect();
    conformance::check_concurrent(stores, 25, "sqlite handles");
}

/// Register `BY_CHILD_EACH` workers in the registry at `BY_CHILD_REGISTRY`
/// as `BY_CHILD_NAME`. Run only as a child of the test below.
#[test]
#[ignore = "a child process of the concurrent registration test"]
fn registry_child() {
    let (Some(path), Some(name), Some(each)) = (
        std::env::var_os("BY_CHILD_REGISTRY"),
        std::env::var("BY_CHILD_NAME").ok(),
        std::env::var("BY_CHILD_EACH").ok(),
    ) else {
        return;
    };
    let registry = LocalRegistry::open(PathBuf::from(path)).unwrap();
    for i in 0..each.parse::<usize>().unwrap() {
        let mut service = Service::new(services::KIND_WORKER, ServiceOwner::this_process())
            .with_id(format!("{name}-{i}"));
        service.lease_until_ms = u64::MAX / 2;
        registry.register(&service, 1).unwrap();
    }
}

#[test]
fn processes_registering_at_once_on_one_file_all_land() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registry.db");
    let children: Vec<_> = (0..4)
        .map(|n| {
            Command::new(std::env::current_exe().unwrap())
                .args(["registry_child", "--exact", "--ignored", "--quiet"])
                .env("BY_CHILD_REGISTRY", &path)
                .env("BY_CHILD_NAME", format!("p{n}"))
                .env("BY_CHILD_EACH", "20")
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let registry = LocalRegistry::open(&path).unwrap();
    let all = registry.all().unwrap();
    assert_eq!(all.len(), 80);
    let mut seqs: Vec<u64> = all.iter().map(|s| s.seq).collect();
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(seqs.len(), 80, "each change has its own number");
    // Every child has exited: its records' owner is gone, and they are
    // expired at once, whatever their lease says.
    let expired = services::expire(&registry, 2).unwrap();
    assert_eq!(expired.len(), 80);
}

/// Start `sleep 600` in a process group of its own, register it as a
/// connector gateway this process supervises, with the group to reclaim,
/// then print its pid and wait to be killed. Run only as a child.
#[test]
#[ignore = "a child process of the reclamation test"]
// The leaked process is meant to outlive this one, which is killed.
#[allow(clippy::zombie_processes)]
fn owner_child() {
    let Some(root) = std::env::var_os("BY_CHILD_ROOT") else {
        return;
    };
    use std::os::unix::process::CommandExt;
    let yard = Yard::open(root).unwrap();
    let leaked = Command::new("sleep")
        .arg("600")
        .process_group(0)
        .spawn()
        .unwrap();
    let service = Service::new(
        services::KIND_CONNECTOR_GATEWAY,
        ServiceOwner::this_process(),
    )
    .with("issuer", "branchyard:local:test")
    .with_endpoint(Endpoint::url("http://127.0.0.1:9/mcp"))
    .with_reclaim(Reclaim::process(leaked.id(), true).unwrap());
    let _held = yard
        .register_service(service, Duration::from_secs(3600))
        .unwrap();
    println!("leaked {}", leaked.id());
    let mut line = String::new();
    // Until killed: stdin stays open.
    let _ = std::io::stdin().read_line(&mut line);
}

#[test]
fn a_killed_owners_leaked_process_is_reclaimed_and_nothing_else() {
    let f = Fixture::new();
    let mut owner = Command::new(std::env::current_exe().unwrap())
        .args([
            "owner_child",
            "--exact",
            "--ignored",
            "--nocapture",
            "--quiet",
        ])
        .env("BY_CHILD_ROOT", &f.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = std::io::BufReader::new(owner.stdout.take().unwrap());
    let leaked: u32 = loop {
        let mut line = String::new();
        assert!(
            std::io::BufRead::read_line(&mut out, &mut line).unwrap() > 0,
            "the owner exited early"
        );
        if let Some(pid) = line.trim().strip_prefix("leaked ") {
            break pid.parse().unwrap();
        }
    };
    // Live while its owner runs: resolved by what it is, not by a URL.
    let found = f
        .yard
        .resolve_service(
            &Query::kind(services::KIND_CONNECTOR_GATEWAY)
                .require("issuer", "branchyard:local:test"),
        )
        .unwrap()
        .expect("the gateway, registered");
    assert_eq!(found.url(), Some("http://127.0.0.1:9/mcp"));
    assert_eq!(found.owner.pid, owner.id());
    // Nothing is reclaimed while the owner lives, even by a reaper.
    assert!(f.yard.reclaim_services().unwrap().is_empty());
    assert!(wait::alive(leaked));
    // A process with the leaked pid but another start time is one
    // Branchyard did not start: its record is reaped, the process is not
    // signalled.
    let mut reused = Service::new("mcp_server", gone_owner())
        .with_id("not-ours")
        .with_reclaim(Reclaim::Process {
            host: branchyard::process_identity().0,
            pid: leaked,
            start: "1".into(),
            group: None,
        });
    reused.lease_until_ms = u64::MAX / 2;
    f.yard.services().unwrap().register(&reused, 1).unwrap();
    let reaped = f.yard.reclaim_services().unwrap();
    assert_eq!(reaped.len(), 1);
    assert_eq!(
        reaped[0].outcome,
        Outcome::Done(format!("process {leaked} had already exited"))
    );
    assert!(wait::alive(leaked));

    owner.kill().unwrap();
    owner.wait().unwrap();
    assert!(wait::alive(leaked), "the owner's group outlived it");
    let reaped = f.yard.reclaim_services().unwrap();
    assert_eq!(
        reaped
            .iter()
            .map(|r| (r.service.id.as_str(), r.outcome.clone()))
            .collect::<Vec<_>>(),
        [(
            found.id.as_str(),
            Outcome::Done(format!("stopped pid {leaked}"))
        )]
    );
    wait::until("the leaked process to stop", || !wait::alive(leaked));
    let row = f.yard.services().unwrap().get(&found.id).unwrap().unwrap();
    assert_eq!(row.state, ServiceState::Reclaimed);
    assert!(f
        .yard
        .resolve_service(&Query::kind(services::KIND_CONNECTOR_GATEWAY))
        .unwrap()
        .is_none());
    // Reclaimed once.
    assert!(f.yard.reclaim_services().unwrap().is_empty());
}

/// `BRANCHYARD_SSH` for the recipe's machine: the fake ssh, with the
/// "VM"'s `HOME` under `dir`.
fn fake_ssh(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir.join("vm-home")).unwrap();
    let wrapper = dir.join("ssh");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nFAKE_SSH_HOME='{}' FAKE_SSH_HOSTS=vm.test exec python3 '{FAKE_SSH}' \"$@\"\n",
            dir.join("vm-home").display()
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    wrapper
}

#[test]
fn an_expired_recipe_machine_is_destroyed_through_its_recorded_destroy() {
    let f = Fixture::new();
    let scripts = f.dir.join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    let log = f.dir.join("recipe.log");
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
    let result = format!(
        r#"{{"schemaVersion":1,"connection":{{"type":"ssh","target":{{"host":"vm.test","username":"dev"}},"projectRoot":"{}"}}}}"#,
        f.dir.join("vm").display()
    );
    let options = RecipeOptions {
        name: "devbox".into(),
        create: write(
            "create.sh",
            &format!(
                "mkdir -p '{}'\necho '{result}'\n",
                f.dir.join("vm").display()
            ),
        ),
        destroy: Some(write("destroy.sh", "cat >/dev/null\n")),
        ..RecipeOptions::default()
    };
    // The machine as the engine makes one for a turn: the same recipe, its
    // record in the repository's `recipes` directory.
    let ssh = fake_ssh(&f.dir);
    let recipe = branchyard_recipe::Recipe::new(&options.name, &f.root, &options.create)
        .with_destroy(options.destroy.as_deref());
    let machines = branchyard_recipe::RecipeProvider::new(recipe, ssh.display().to_string())
        .with_state_dir(f.root.join(".branchyard/recipes"));
    machines.ensure(&SandboxSpec::new("by-ghost-1")).unwrap();
    assert!(machines.result("by-ghost-1").is_some());

    std::env::set_var("BRANCHYARD_SSH", &ssh);
    let mut service = Service::new(
        services::KIND_RECIPE_MACHINE,
        gone_owner().for_branch("ghost"),
    )
    .with("sandbox", "by-ghost-1")
    .with_reclaim(Reclaim::Sandbox {
        root: f.root.clone(),
        branch: "ghost".into(),
        provider: Box::new(Provider::Recipe(options.clone())),
        sandbox: "by-ghost-1".into(),
    });
    service.lease_until_ms = u64::MAX / 2;
    let registered = f.yard.services().unwrap().register(&service, 1).unwrap();

    let reaped = f.yard.reclaim_services().unwrap();
    assert_eq!(reaped.len(), 1, "{reaped:?}");
    assert_eq!(
        reaped[0].outcome,
        Outcome::Done("destroyed machine by-ghost-1 (recipe devbox)".into())
    );
    let lines = fs::read_to_string(&log).unwrap();
    assert_eq!(
        lines.lines().collect::<Vec<_>>(),
        ["create by-ghost-1", "destroy by-ghost-1"]
    );
    assert!(!f.root.join(".branchyard/recipes/by-ghost-1.json").exists());
    let row = f
        .yard
        .services()
        .unwrap()
        .get(&registered.id)
        .unwrap()
        .unwrap();
    assert_eq!(row.state, ServiceState::Reclaimed);
    assert_eq!(
        row.note.as_deref(),
        Some("destroyed machine by-ghost-1 (recipe devbox)")
    );
}

#[test]
fn a_gone_pool_keepers_slots_are_reclaimed() {
    let f = Fixture::new();
    // What a filler that stopped left: a slot directory with no record.
    let orphan = f.root.join(".branchyard/pool/slot-orphan");
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("half-made"), "x").unwrap();
    let mut keeper =
        Service::new(services::KIND_POOL_KEEPER, gone_owner()).with_reclaim(Reclaim::PoolSlots {
            root: f.root.clone(),
        });
    keeper.lease_until_ms = u64::MAX / 2;
    f.yard.services().unwrap().register(&keeper, 1).unwrap();
    let reaped = f.yard.reclaim_services().unwrap();
    assert_eq!(
        reaped.iter().map(|r| r.outcome.clone()).collect::<Vec<_>>(),
        [Outcome::Done("removed 1 pool slot(s)".into())]
    );
    assert!(!orphan.exists());
}

#[test]
fn a_record_whose_owner_left_is_never_reclaimed() {
    let f = Fixture::new();
    let leaked = Command::new("sleep").arg("600").spawn().unwrap();
    let pid = leaked.id();
    let held = f
        .yard
        .register_service(
            Service::new(services::KIND_MCP_SERVER, ServiceOwner::this_process())
                .with_reclaim(Reclaim::process(pid, false).unwrap()),
            Duration::from_secs(60),
        )
        .unwrap();
    let id = held.id();
    drop(held);
    let row = f.yard.services().unwrap().get(&id).unwrap().unwrap();
    assert_eq!(row.state, ServiceState::Left);
    assert!(f.yard.reclaim_services().unwrap().is_empty());
    assert!(wait::alive(pid));
    let mut leaked = leaked;
    leaked.kill().unwrap();
    leaked.wait().unwrap();
}
