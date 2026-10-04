//! Qualification against a real Agent Substrate cluster. Ignored by
//! default; `docs/testing-live.md` section 6 says how to run them. Each test
//! reads its target from the environment and panics naming the first
//! variable that is missing:
//!
//! - `BY_SUBSTRATE_ENDPOINT`: the `Control` API, `https://host:port` (or
//!   `http://` on loopback, such as a port-forward).
//! - `BY_SUBSTRATE_ROUTER`: the router URL template with `{atespace}` and
//!   `{actor}`.
//! - `BY_SUBSTRATE_ATESPACE`: an existing atespace.
//! - `BY_SUBSTRATE_TEMPLATE`: the bridge template's name.
//! - `BY_SUBSTRATE_KEY`: the signing key whose public half the template holds.
//! - `BY_SUBSTRATE_WORKDIR`: a writable directory in the actor, for the
//!   conformance checks and the worktree (for example `/workspace`).
//!
//! Optional: `BY_SUBSTRATE_CA`, `BY_SUBSTRATE_CLIENT_CERT`,
//! `BY_SUBSTRATE_CLIENT_KEY` and `BY_SUBSTRATE_ROUTER_CA` (PEM files, as the
//! `--substrate-*` flags of the same names), and `BY_SUBSTRATE_INSECURE=1`.
//!
//! `create_bridge_template` also reads `BY_SUBSTRATE_IMAGE` (pinned by
//! digest), `BY_SUBSTRATE_BRIDGE` (the bridge's path in the image),
//! `BY_SUBSTRATE_SANDBOX_CONFIG` and `BY_SUBSTRATE_STORAGE`, and optionally
//! `BY_SUBSTRATE_RUN_AS` (`UID:GID` for execs) and
//! `BY_SUBSTRATE_BRIDGE_TLS_CERT` and `BY_SUBSTRATE_BRIDGE_TLS_KEY` (paths
//! in the image, for a router that passes TLS through).

#![allow(clippy::let_underscore_must_use, clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use branchyard_bridge::Signer;
use branchyard_sandbox::conformance::{self, Setup};
use branchyard_sandbox::{ExecSpec, Process, SandboxProvider, SandboxSpec};
use branchyard_substrate::template::{bridge_template_with, BridgeOptions, BridgeTemplate};
use branchyard_substrate::{pb, transfer, Config, SubstrateProvider};

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}; see docs/testing-live.md"))
}

fn optional(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn config() -> Config {
    let mut config = Config::new(
        var("BY_SUBSTRATE_ENDPOINT"),
        var("BY_SUBSTRATE_ATESPACE"),
        var("BY_SUBSTRATE_TEMPLATE"),
        var("BY_SUBSTRATE_ROUTER"),
    )
    .signer(Signer::read(Path::new(&var("BY_SUBSTRATE_KEY"))).unwrap());
    config.ready_timeout = Duration::from_secs(300);
    config.ca = optional("BY_SUBSTRATE_CA").map(PathBuf::from);
    config.client_cert = optional("BY_SUBSTRATE_CLIENT_CERT").map(PathBuf::from);
    config.client_key = optional("BY_SUBSTRATE_CLIENT_KEY").map(PathBuf::from);
    config.router_ca = optional("BY_SUBSTRATE_ROUTER_CA").map(PathBuf::from);
    config.insecure = optional("BY_SUBSTRATE_INSECURE").as_deref() == Some("1");
    config
}

fn provider() -> SubstrateProvider {
    SubstrateProvider::connect(config()).unwrap()
}

fn unique(what: &str) -> String {
    format!("by-qual-{what}-{}", branchyard_support::time::now_ms())
}

#[test]
#[ignore = "creates an actor template on a real cluster"]
fn create_bridge_template() {
    let signer = Signer::read(Path::new(&var("BY_SUBSTRATE_KEY"))).unwrap();
    let run_as = optional("BY_SUBSTRATE_RUN_AS").map(|text| {
        let (uid, gid) = text
            .split_once(':')
            .expect("BY_SUBSTRATE_RUN_AS is UID:GID");
        (uid.parse().unwrap(), gid.parse().unwrap())
    });
    let tls = optional("BY_SUBSTRATE_BRIDGE_TLS_CERT")
        .map(|cert| (cert, var("BY_SUBSTRATE_BRIDGE_TLS_KEY")));
    let template = bridge_template_with(
        &var("BY_SUBSTRATE_ATESPACE"),
        &BridgeTemplate {
            name: var("BY_SUBSTRATE_TEMPLATE"),
            image: var("BY_SUBSTRATE_IMAGE"),
            bridge: var("BY_SUBSTRATE_BRIDGE"),
            public_key: signer.public_key(),
            sandbox_class: pb::SandboxClass::Gvisor,
            sandbox_config: var("BY_SUBSTRATE_SANDBOX_CONFIG"),
            storage_location: var("BY_SUBSTRATE_STORAGE"),
        },
        &BridgeOptions { tls, run_as },
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let created = runtime.block_on(async {
        let mut client = pb::control_client::ControlClient::new(config().channel()?);
        client
            .create_actor_template(pb::CreateActorTemplateRequest {
                actor_template: Some(template),
            })
            .await
            .map_err(Into::into)
    });
    let created: Result<_, Box<dyn std::error::Error>> = created;
    println!("{:#?}", created.unwrap().into_inner().metadata);
}

#[test]
#[ignore = "needs a Substrate cluster"]
fn cluster_conformance() {
    let provider = provider();
    assert!(
        provider.template_capabilities().unwrap().exec,
        "the template does not run the bridge"
    );
    let mut setup = Setup::without_mounts(unique("conf"), var("BY_SUBSTRATE_WORKDIR"));
    setup.timeout = Duration::from_secs(120);
    conformance::run_all(&provider, &setup);
}

#[test]
#[ignore = "needs a Substrate cluster"]
fn cluster_credentials_are_per_attempt() {
    let provider = provider();
    let name = unique("attempts");
    provider.ensure(&SandboxSpec::new(&name)).unwrap();
    let spec = ExecSpec {
        argv: vec!["true".into()],
        cwd: "/".into(),
        env: Default::default(),
    };
    let first = provider.endpoint(&name).unwrap();
    first.exec(&spec).unwrap().wait().unwrap();
    provider.begin_attempt(&name, "second").unwrap();
    let refused = first.exec(&spec).map(|_| ());
    provider.destroy(&name).unwrap();
    assert!(refused.is_err(), "a superseded credential was accepted");
}

#[test]
#[ignore = "needs a Substrate cluster"]
fn cluster_worktree_round_trip() {
    let dir = std::env::temp_dir().join(unique("repo"));
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(&dir)
            .status()
            .unwrap()
            .success())
    };
    git(&["init", "-q"]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=q",
        "-c",
        "user.email=q@localhost",
        "commit",
        "-q",
        "-m",
        "one",
    ]);
    let provider = provider();
    let name = unique("transfer");
    provider.ensure(&SandboxSpec::new(&name)).unwrap();
    let endpoint = provider.endpoint(&name).unwrap();
    let guest = PathBuf::from(var("BY_SUBSTRATE_WORKDIR")).join(&name);
    let pushed = transfer::push(&endpoint, &dir, &guest).unwrap();
    let mut edit = endpoint
        .exec(&ExecSpec {
            argv: vec!["sh".into(), "-c".into(), "printf two > a.txt".into()],
            cwd: guest.clone(),
            env: Default::default(),
        })
        .unwrap();
    assert!(edit.wait().unwrap().success());
    let pulled = transfer::pull(&endpoint, &pushed, &dir).unwrap();
    provider.destroy(&name).unwrap();
    assert!(pulled.changed);
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "two");
    let _ = std::fs::remove_dir_all(&dir);
}
