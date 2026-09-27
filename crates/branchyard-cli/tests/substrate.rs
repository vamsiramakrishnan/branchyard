//! `by run --provider substrate` end to end: the built `by` binary against
//! the fake Substrate cluster in this test process, whose actor runs the
//! fake ACP agent behind a real bridge; then `by merge`. Hermetic: no real
//! harness, no cluster. Requires `git` and `sh`.

use std::fs;
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
    command
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("NO_COLOR", "1")
        .env("PAGER", "cat");
    for var in [
        "BRANCHYARD_DELEGATION",
        "BRANCHYARD_BRANCH",
        "BRANCHYARD_ROOT",
        "BRANCHYARD_BY",
        "BRANCHYARD_REMOTE",
    ] {
        command.env_remove(var);
    }
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
        let _ = fs::remove_dir_all(&dir);
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
        let _ = fs::remove_dir_all(&self.dir);
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
        let mut child = Command::new(BY)
            .current_dir(dir)
            .args(["serve", "--listen", "127.0.0.1:0", "--quiet", "--repo"])
            .arg(format!("app={}", root.display()))
            .arg("--data-dir")
            .arg(&data)
            .args(extra)
            .env_remove("BRANCHYARD_REMOTE")
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
        let _ = self.child.kill();
        let _ = self.child.wait();
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
