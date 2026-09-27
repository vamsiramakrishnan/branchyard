//! A fake cluster whose actors run the real bridge, for hermetic tests.
//! Requires `git` and `sh`.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Once, OnceLock};
use std::time::Duration;

use branchyard_bridge::Signer;
use branchyard_substrate::fake::FakeCluster;
use branchyard_substrate::template::{bridge_template, BridgeTemplate};
use branchyard_substrate::{pb, Config, SubstrateProvider};

pub const ATESPACE: &str = "tenant-a";
pub const TEMPLATE: &str = "by-bridge";

static COUNTER: AtomicU64 = AtomicU64::new(0);
static HERMETIC: Once = Once::new();

/// The `branchyard-bridge` binary, built once per test binary into this
/// build's target directory; cargo exposes a binary's path only to its own
/// package's tests.
pub fn bridge_binary() -> &'static Path {
    static BRIDGE: OnceLock<PathBuf> = OnceLock::new();
    BRIDGE.get_or_init(|| {
        let exe = std::env::current_exe().unwrap();
        let profile_dir = exe.parent().and_then(Path::parent).unwrap().to_path_buf();
        let target_dir = profile_dir.parent().unwrap();
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut command = Command::new(cargo);
        command
            .args(["build", "--quiet", "--offline", "--manifest-path"])
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"))
            .args(["-p", "branchyard-bridge", "--bin", "branchyard-bridge"])
            .env("CARGO_TARGET_DIR", target_dir);
        match profile_dir.file_name().and_then(|n| n.to_str()) {
            Some("debug") => {}
            Some("release") => {
                command.arg("--release");
            }
            Some(other) => {
                command.args(["--profile", other]);
            }
            None => panic!("unexpected test binary location {}", exe.display()),
        }
        assert!(
            command.status().unwrap().success(),
            "building branchyard-bridge failed"
        );
        let bridge = profile_dir.join("branchyard-bridge");
        assert!(bridge.is_file(), "{} was not built", bridge.display());
        bridge
    })
}

/// A fresh temporary directory, removed on drop.
pub struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "branchyard-substrate-test-{}-{}-{name}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Scratch(fs::canonicalize(dir).unwrap())
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A fake cluster with the bridge template, and the key its bridges trust.
/// Fields drop in order: the cluster (and its bridges) before the scratch
/// directory.
pub struct Cluster {
    pub fake: FakeCluster,
    pub key: PathBuf,
    pub scratch: Scratch,
}

impl Cluster {
    pub fn start(name: &str) -> Cluster {
        // Bridges inherit this environment, so git in the fake actors must
        // not read the host's configuration either.
        HERMETIC.call_once(|| {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        });
        let scratch = Scratch::new(name);
        let key = scratch.path("bridge.key");
        let signer = Signer::write(&key).unwrap();
        let fake = FakeCluster::start(
            ATESPACE,
            Some(bridge_binary().to_path_buf()),
            &scratch.path("cluster"),
        );
        fake.add_template(bridge_template(
            ATESPACE,
            &BridgeTemplate {
                name: TEMPLATE.into(),
                image: "registry.example/branchyard@sha256:00".into(),
                bridge: "/usr/local/bin/branchyard-bridge".into(),
                public_key: signer.public_key(),
                sandbox_class: pb::SandboxClass::Gvisor,
                sandbox_config: "gvisor".into(),
                storage_location: "gs://bucket/branchyard".into(),
            },
        ));
        Cluster { fake, key, scratch }
    }

    pub fn config(&self) -> Config {
        let mut config = Config::new(self.fake.endpoint(), ATESPACE, TEMPLATE, self.fake.router())
            .signer(Signer::read(&self.key).unwrap());
        config.ready_timeout = Duration::from_secs(20);
        config
    }

    pub fn provider(&self) -> SubstrateProvider {
        SubstrateProvider::connect(self.config()).unwrap()
    }
}

/// Whether `pid` is a live (not zombie) process on this host.
pub fn alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        let state = stat
            .rsplit(')')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .next();
        !matches!(state, Some("Z") | Some("X"))
    })
}

pub fn wait_gone(pid: u32) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "pid {pid} is still running"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
