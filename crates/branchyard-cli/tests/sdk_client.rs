#![allow(clippy::let_underscore_must_use, clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
//! The Python HTTP client, `sdk/python/branchyard_client.py`, against a
//! real `by serve`: the fake ACP agent is the harness, a fake Anvil
//! packages the connector bundles, delegation is allowed, and
//! `tests/sdk_client_e2e.py` drives a surface's conversation through the
//! client (grants given and narrowed, a branch that lives on, the busy
//! policies, idempotency, typed refusals). Hermetic; skipped without
//! `python3`.

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use branchyard_testkit::fake_agent;

const BY: &str = env!("CARGO_BIN_EXE_by");

/// Anvil as the gateway's packager only: `anvil package harness` writes a
/// skill, `anvil connectors index` an index. Nothing is called through it.
const FAKE_ANVIL: &str = r##"#!/bin/sh
case "$1 $2" in
"package harness") mkdir -p "$5" && echo "# $(basename "$3")" > "$5/SKILL.md" ;;
"connectors index") echo "# Connectors" > "$6" ;;
*) echo "fake anvil: $*" >&2; exit 2 ;;
esac
"##;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn python() -> Option<&'static str> {
    let works = Command::new("python3")
        .args(["-c", "pass"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    works.then_some("python3")
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// `by serve` on a free loopback port, killed on drop.
struct Served {
    child: Child,
    url: String,
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn serve(root: &Path, config: &Path, log: &Path) -> Served {
    let mut command = Command::new(BY);
    branchyard_testkit::hermetic(&mut command)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("NO_COLOR", "1")
        .env(
            "BRANCHYARD_USER_CONFIG",
            "/nonexistent/branchyard-config.toml",
        )
        .args(["serve", "-c"])
        .arg(config)
        .args(["--listen", "127.0.0.1:0", "--quiet", "--allow-delegation"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(fs::File::create(log).unwrap());
    let mut child = command.spawn().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = line
        .trim()
        .strip_prefix("listening on ")
        .unwrap_or_else(|| panic!("server did not start: {line:?}"))
        .to_owned();
    Served { child, url }
}

#[test]
fn the_python_client_drives_a_granted_conversation_whose_child_inherits_a_narrower_grant() {
    let Some(python) = python() else {
        eprintln!("skipped: no python3");
        return;
    };
    let scratch = branchyard_testkit::Scratch::new("sdk-client");
    let dir = scratch.path();
    let root = dir.join("app");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "user.email", "test@localhost"]);
    git(&root, &["config", "commit.gpgsign", "false"]);
    fs::write(root.join("a.txt"), "one\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "initial"]);

    let bundles = dir.join("bundles");
    for connector in ["github", "linear"] {
        fs::create_dir_all(bundles.join(connector)).unwrap();
        fs::write(
            bundles.join(connector).join("air.yaml"),
            format!("service: {connector}\n"),
        )
        .unwrap();
    }
    let anvil = dir.join("fake-anvil");
    fs::write(&anvil, FAKE_ANVIL).unwrap();
    fs::set_permissions(&anvil, fs::Permissions::from_mode(0o755)).unwrap();

    let data = dir.join("data");
    let config = serde_json::json!({
        "repos": { "app": root },
        "data_dir": data,
        "harness_commands": { "gemini-cli": [fake_agent!()] },
        "allow_delegation": true,
        "connectors": {
            "gateway": "http://127.0.0.1:9/mcp",
            "issuer": "https://by.example",
            "bundles": bundles,
            "anvil": [anvil],
            "run_gateway": false
        }
    });
    let config_path = dir.join("config.json");
    fs::write(&config_path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
    let log = dir.join("server.log");
    let server = serve(&root, &config_path, &log);

    let script = repo_root().join("tests/sdk_client_e2e.py");
    let out = Command::new(python)
        .arg("-I")
        .arg(&script)
        .current_dir(dir)
        .env("BRANCHYARD_URL", &server.url)
        .env("BRANCHYARD_TOKEN_FILE", data.join("token"))
        .env("BRANCHYARD_REPO", "app")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let server_log = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        out.status.success() && stdout.trim_end().ends_with("ok"),
        "python client e2e failed\nstdout: {stdout}\nstderr: {stderr}\nserver log:\n{server_log}"
    );
    drop(server);
}
