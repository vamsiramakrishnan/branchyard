//! Qualification against a real Agent Substrate cluster. Ignored by
//! default; `docs/testing-live.md` section 6 says how to run them. Each test
//! reads its target from the environment and panics naming the first
//! variable that is missing:
//!
//! - `BY_SUBSTRATE_ENDPOINT`: the TLS `Control` API, `https://host:port`.
//! - `BY_SUBSTRATE_ROUTER`: the router base URL.
//! - `BY_SUBSTRATE_ATESPACE`: an existing atespace.
//! - `BY_SUBSTRATE_TEMPLATE`: the bridge template's name.
//! - `BY_SUBSTRATE_KEY`: the signing key whose public half the template holds.
//! - `BY_SUBSTRATE_WORKDIR`: a writable directory in the actor, for the
//!   conformance checks and the worktree (for example `/workspace`).
//!
//! Optional: `BY_SUBSTRATE_API_TOKEN_FILE`, `BY_SUBSTRATE_SERVER_NAME`,
//! `BY_SUBSTRATE_CA`, `BY_SUBSTRATE_CLIENT_CERT`,
//! `BY_SUBSTRATE_CLIENT_KEY` and `BY_SUBSTRATE_ROUTER_CA` (PEM files, as the
//! `--substrate-*` flags of the same names), and `BY_SUBSTRATE_INSECURE=1`.
//!
//! `create_bridge_template` also reads `BY_SUBSTRATE_IMAGE` (pinned by
//! digest), `BY_SUBSTRATE_BRIDGE` (the bridge's path in the image),
//! `BY_SUBSTRATE_SANDBOX_CONFIG` and `BY_SUBSTRATE_STORAGE`, and optionally
//! `BY_SUBSTRATE_RUN_AS` (`UID:GID` for execs), `BY_SUBSTRATE_CPU`,
//! `BY_SUBSTRATE_MEMORY`, `BY_SUBSTRATE_WORKER_LABELS` (`NAME=VALUE` pairs
//! separated by commas), and
//! `BY_SUBSTRATE_BRIDGE_TLS_CERT` and `BY_SUBSTRATE_BRIDGE_TLS_KEY` (paths
//! in the image, for a router that passes TLS through).

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use branchyard_bridge::Signer;
use branchyard_sandbox::conformance::{self, Setup};
use branchyard_sandbox::{ExecSpec, Process, SandboxProvider, SandboxSpec};
use branchyard_substrate::template::{BridgeOptions, BridgeTemplate};
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
    config.api_token_file = optional("BY_SUBSTRATE_API_TOKEN_FILE").map(PathBuf::from);
    config.server_name = optional("BY_SUBSTRATE_SERVER_NAME");
    config.insecure = optional("BY_SUBSTRATE_INSECURE").as_deref() == Some("1");
    config
}

fn provider() -> SubstrateProvider {
    SubstrateProvider::connect(config()).unwrap()
}

fn unique(what: &str) -> String {
    format!(
        "by-qual-{what}-{}",
        branchyard_support::new_ulid().unwrap().to_ascii_lowercase()
    )
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
    let spec = BridgeTemplate {
        name: var("BY_SUBSTRATE_TEMPLATE")
            .rsplit('/')
            .next()
            .unwrap()
            .into(),
        image: var("BY_SUBSTRATE_IMAGE"),
        bridge: var("BY_SUBSTRATE_BRIDGE"),
        public_key: signer.public_key(),
        sandbox_class: pb::SandboxClass::Gvisor,
        sandbox_config: var("BY_SUBSTRATE_SANDBOX_CONFIG"),
        storage_location: var("BY_SUBSTRATE_STORAGE"),
    };
    let options = BridgeOptions {
        tls,
        run_as,
        resources: branchyard_substrate::template::TemplateResources {
            cpu: optional("BY_SUBSTRATE_CPU"),
            memory: optional("BY_SUBSTRATE_MEMORY"),
        },
        worker_selector: optional("BY_SUBSTRATE_WORKER_LABELS")
            .map(|labels| {
                labels
                    .split(',')
                    .map(|label| {
                        let (name, value) =
                            label.split_once('=').expect("worker label is NAME=VALUE");
                        (name.to_owned(), value.to_owned())
                    })
                    .collect()
            })
            .unwrap_or_default(),
    };
    let provider = provider();
    let plan = provider.plan_setup(&spec, &options).unwrap();
    println!(
        "{:#?}",
        provider
            .apply_setup(&plan, |event| println!("{event:?}"))
            .unwrap()
    );
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
        writable: None,
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
            writable: None,
        })
        .unwrap();
    assert!(edit.wait().unwrap().success());
    let pulled = transfer::pull(&endpoint, &pushed, &dir).unwrap();
    provider.destroy(&name).unwrap();
    assert!(pulled.changed);
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "two");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
#[ignore = "creates, pauses, snapshots and branches actors on a real cluster"]
fn cluster_pause_checkpoint_and_branch() {
    use branchyard_sandbox::{ProviderError, SandboxState};

    let provider = provider();
    let parent = unique("snapshot-parent");
    let child = unique("snapshot-child");
    let path = PathBuf::from(var("BY_SUBSTRATE_WORKDIR")).join("snapshot-proof.txt");
    let mut checkpoint = None;
    let result = (|| -> Result<(), ProviderError> {
        provider.ensure(&SandboxSpec::new(&parent))?;
        provider.put_file(&parent, &path, 0o600, &mut &b"parent state\n"[..])?;
        provider.pause(&parent)?;
        assert_eq!(
            provider.inspect(&parent)?.unwrap().state,
            SandboxState::Paused
        );
        provider.resume(&parent)?;
        let mut before = Vec::new();
        provider.get_file(&parent, &path, &mut before)?;
        assert_eq!(before, b"parent state\n");
        provider.pause(&parent)?;
        let captured = provider.checkpoint(&parent, &provider.capabilities().checkpoint[0])?;
        checkpoint = Some(captured.clone());
        assert_eq!(
            provider.inspect(&parent)?.unwrap().state,
            SandboxState::Stopped
        );
        provider.branch(&captured, &SandboxSpec::new(&child))?;
        provider.resume(&child)?;
        let mut inherited = Vec::new();
        provider.get_file(&child, &path, &mut inherited)?;
        assert_eq!(inherited, b"parent state\n");
        provider.put_file(&child, &path, 0o600, &mut &b"child state\n"[..])?;
        provider.resume(&parent)?;
        let mut unchanged = Vec::new();
        provider.get_file(&parent, &path, &mut unchanged)?;
        assert_eq!(unchanged, b"parent state\n");
        assert_ne!(
            provider.handle(&parent)?.unwrap().uid,
            provider.handle(&child)?.unwrap().uid
        );
        Ok(())
    })();
    let child_cleanup = provider.destroy(&child);
    let parent_cleanup = provider.destroy(&parent);
    let checkpoint_cleanup = checkpoint
        .as_ref()
        .map(|value| provider.release_checkpoint(value));
    result.unwrap();
    child_cleanup.unwrap();
    parent_cleanup.unwrap();
    if let Some(released) = checkpoint_cleanup {
        released.unwrap();
    }
}
