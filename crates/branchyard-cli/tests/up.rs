//! `by up` through the built `by` (docs/up.md), hermetic: a fake harness on
//! a temporary `PATH`, a fake `npm` that installs it, a user file that
//! allows or refuses installs, secrets by variable name, a pinned gateway
//! with no Anvil, and `--plan`, `--yes`, `--json`, the exit status, the
//! text report and the refusal inside a harness. Nothing is installed from
//! the network and no real harness runs. Requires `git`, `sh` and `python3`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

const BY: &str = env!("CARGO_BIN_EXE_by");

/// Temporary directories: `bin` (on `PATH`, with the fakes), `home` (with
/// `.local/bin` on `PATH` too), the user configuration and a repository.
struct World {
    _dir: tempfile::TempDir,
    base: PathBuf,
    bin: PathBuf,
    home: PathBuf,
    user: PathBuf,
    repo: PathBuf,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let (bin, home, repo) = (base.join("bin"), base.join("home"), base.join("repo"));
        for d in [&bin, &home.join(".local/bin"), &repo] {
            fs::create_dir_all(d).unwrap();
        }
        let world = World {
            user: base.join("user/config.toml"),
            _dir: dir,
            base,
            bin,
            home,
            repo,
        };
        branchyard_testkit::system_tools(&world.base.join("tools")).unwrap();
        world.git(&["init", "-q", "-b", "main"]);
        world.git(&["config", "user.name", "Test"]);
        world.git(&["config", "user.email", "test@localhost"]);
        fs::write(world.repo.join("a.txt"), "one\n").unwrap();
        fs::write(world.repo.join(".gitignore"), "branchyard.toml\n").unwrap();
        world.git(&["add", "."]);
        world.git(&["commit", "-q", "-m", "initial"]);
        world
    }

    fn script(&self, name: &str, text: &str) -> PathBuf {
        let path = self.bin.join(name);
        write_exec(&path, text);
        path
    }

    fn path_var(&self) -> String {
        format!(
            "{}:{}:{}",
            self.bin.display(),
            self.home.join(".local/bin").display(),
            self.base.join("tools").display()
        )
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command.env_clear();
        // Under `cargo llvm-cov` the `by` run here is instrumented: keep
        // the profile path, so what it covers counts.
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        branchyard_testkit::hermetic(&mut command)
            .current_dir(&self.repo)
            .env("PATH", self.path_var())
            .env("HOME", &self.home)
            .env("BRANCHYARD_USER_CONFIG", &self.user)
            .env("BRANCHYARD_TRUST_FILE", self.base.join("trusted.json"))
            .env("BRANCHYARD_HARNESS_DIRS", "$HOME/.local/bin")
            .env("BRANCHYARD_HARNESS_TIMEOUT", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env("FAKE_LOG", self.base.join("calls.log"));
        command
    }

    fn git(&self, args: &[&str]) {
        let out = self.command("git").args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
    }

    fn by_env(&self, args: &[&str], vars: &[(&str, &str)]) -> Output {
        let mut command = self.command(BY);
        for (k, v) in vars {
            command.env(k, v);
        }
        command.args(args).stdin(Stdio::null()).output().unwrap()
    }

    fn by(&self, args: &[&str]) -> Output {
        self.by_env(args, &[])
    }

    /// `by up --json <args>`: the steps, and the exit code.
    fn up(&self, args: &[&str], vars: &[(&str, &str)]) -> (Value, Option<i32>) {
        let mut all = vec!["up", "--json"];
        all.extend(args);
        let out = self.by_env(&all, vars);
        let stdout = text(&out.stdout);
        let value: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
            panic!(
                "by up printed no JSON ({e})\nstdout:\n{stdout}\nstderr:\n{}",
                text(&out.stderr)
            )
        });
        (value, out.status.code())
    }

    fn project_file(&self, body: &str) {
        fs::write(self.repo.join("branchyard.toml"), body).unwrap();
    }

    fn user_config(&self, body: &str) {
        fs::create_dir_all(self.user.parent().unwrap()).unwrap();
        fs::write(&self.user, body).unwrap();
    }

    /// Lines of the fakes' call log that contain `needle`.
    fn calls(&self, needle: &str) -> Vec<String> {
        fs::read_to_string(self.base.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains(needle))
            .map(str::to_owned)
            .collect()
    }

    /// A fake Codex in `$HOME/.local/bin`: prints `codex-cli VERSION`, and
    /// answers `login status` with what `codex.status` beside it says
    /// (default: not logged in).
    fn fake_codex(&self, version: &str) -> PathBuf {
        let path = self.home.join(".local/bin/codex");
        write_exec(&path, &codex_script(version));
        path
    }

    fn codex_logged_in(&self) {
        fs::write(
            self.home.join(".local/bin/codex.status"),
            "Logged in using ChatGPT\n",
        )
        .unwrap();
    }

    /// A fake `npm` whose `install -g @openai/codex[@VERSION]` records the
    /// call and puts a fake Codex into `$HOME/.local/bin`.
    fn fake_npm(&self) {
        let codex = branchyard_recipe::quote(&codex_script("@VERSION@"));
        self.script(
            "npm",
            &format!(
                r#"#!/bin/sh
echo "npm $*" >> "$FAKE_LOG"
[ "$1" = install ] && [ "$2" = -g ] || exit 2
package=$3
case $package in
  @*/*@*) version=${{package##*@}};;
  *) version=;;
esac
mkdir -p "$HOME/.local/bin"
printf '%s' {codex} | sed "s/@VERSION@/${{version:-0.150.0}}/" > "$HOME/.local/bin/codex"
chmod +x "$HOME/.local/bin/codex"
echo "added 1 package"
"#
            ),
        );
    }
}

fn codex_script(version: &str) -> String {
    format!(
        r#"#!/bin/sh
echo "codex $*" >> "$FAKE_LOG"
status="$(dirname "$0")/codex.status"
case "$1 $2" in
  "--version "*) echo "codex-cli {version}";;
  "login status") if [ -f "$status" ]; then cat "$status"; else echo "Not logged in"; fi;;
  *) exit 3;;
esac
"#
    )
}

fn write_exec(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The step named `name`, or a panic listing the names.
fn step<'a>(plan: &'a Value, name: &str) -> &'a Value {
    let steps = plan["steps"].as_array().unwrap();
    steps.iter().find(|s| s["name"] == name).unwrap_or_else(|| {
        let names: Vec<&str> = steps.iter().filter_map(|s| s["name"].as_str()).collect();
        panic!("no step {name} in {names:?}")
    })
}

fn names(plan: &Value) -> Vec<&str> {
    plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect()
}

#[test]
fn a_configured_harness_that_is_installed_and_logged_in_is_ready() {
    let w = World::new();
    w.fake_codex("0.150.0");
    w.codex_logged_in();
    w.project_file("[defaults]\nharness = \"codex\"\n");
    let (plan, code) = w.up(&[], &[]);
    assert_eq!(code, Some(0), "{plan}");
    assert_eq!(plan["ready"], true);
    assert_eq!(plan["state"], "ready");
    assert_eq!(names(&plan), ["store", "harness.codex", "login.codex"]);
    assert_eq!(step(&plan, "harness.codex")["state"], "ready");
    assert!(step(&plan, "harness.codex")["detail"]
        .as_str()
        .unwrap()
        .contains("version 0.150.0"));
    assert_eq!(step(&plan, "login.codex")["state"], "ready");
    // The text report says so too.
    let out = w.by(&["up"]);
    assert_eq!(out.status.code(), Some(0));
    let shown = text(&out.stdout);
    assert!(shown.contains("ready: every step is in place"), "{shown}");
    assert!(shown.contains("harness.codex"), "{shown}");
    assert!(!shown.contains("To do"), "{shown}");
}

#[test]
fn a_missing_harness_is_installed_when_the_user_file_allows_and_planned_first() {
    let w = World::new();
    w.fake_npm();
    w.project_file("[defaults]\nharness = \"codex\"\n");
    w.user_config("[harnesses]\ninstall = \"auto\"\n");
    // The plan says what it would run and runs nothing.
    let (plan, code) = w.up(&["--plan"], &[]);
    assert_eq!(code, Some(1), "{plan}");
    let codex = step(&plan, "harness.codex");
    assert_eq!(codex["state"], "planned");
    assert_eq!(codex["action"]["kind"], "install");
    assert!(codex["action"]["plan"]["command"]
        .as_str()
        .unwrap()
        .contains("npm install -g @openai/codex"));
    assert!(w.calls("npm").is_empty());
    assert!(!w.home.join(".local/bin/codex").exists());
    // Applied: installed, and the login is yours.
    let (plan, code) = w.up(&[], &[]);
    assert_eq!(code, Some(1), "{plan}");
    assert_eq!(w.calls("npm install -g").len(), 1, "{:?}", w.calls("npm"));
    assert!(w.home.join(".local/bin/codex").exists());
    assert_eq!(step(&plan, "harness.codex")["state"], "ready");
    assert!(step(&plan, "harness.codex")["detail"]
        .as_str()
        .unwrap()
        .contains("installed with"));
    let login = step(&plan, "login.codex");
    assert_eq!(login["state"], "needs_person");
    assert!(login["todo"].as_str().unwrap().contains("codex"), "{login}");
    assert_eq!(plan["ready"], false);
    // Logged in: ready, and the install is not run again.
    w.codex_logged_in();
    let (plan, code) = w.up(&[], &[]);
    assert_eq!(code, Some(0), "{plan}");
    assert_eq!(plan["ready"], true);
    assert_eq!(w.calls("npm install -g").len(), 1);
    // The install is in the harness log.
    let log = fs::read_to_string(w.user.parent().unwrap().join("harness-events.jsonl")).unwrap();
    assert!(log.contains("\"by\":\"by up\""), "{log}");
}

#[test]
fn without_a_terminal_an_unset_policy_leaves_the_install_to_the_person() {
    let w = World::new();
    w.fake_npm();
    w.project_file("[defaults]\nharness = \"codex\"\n");
    let (plan, code) = w.up(&[], &[]);
    assert_eq!(code, Some(1), "{plan}");
    let codex = step(&plan, "harness.codex");
    assert_eq!(codex["state"], "needs_person");
    assert!(codex["todo"]
        .as_str()
        .unwrap()
        .contains("npm install -g @openai/codex"));
    assert!(codex.get("action").is_none(), "{codex}");
    // `--yes` cannot allow what the policy does not.
    let (plan, _) = w.up(&["--yes"], &[]);
    assert_eq!(step(&plan, "harness.codex")["state"], "needs_person");
    assert!(w.calls("npm").is_empty());
    // A repository's file cannot allow installs either: the loader refuses it.
    w.project_file("[defaults]\nharness = \"codex\"\n[harnesses]\ninstall = \"auto\"\n");
    let out = w.by(&["up", "--json"]);
    assert_eq!(out.status.code(), Some(3));
    // With `--json` the refusal is the error envelope on stdout.
    let error: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("harnesses"),
        "{error}"
    );
    assert!(w.calls("npm").is_empty());
    // The text report lists what to do.
    w.project_file("[defaults]\nharness = \"codex\"\n");
    let out = w.by(&["up"]);
    let shown = text(&out.stdout);
    assert!(shown.contains("To do:"), "{shown}");
    assert!(shown.contains("harness.codex:"), "{shown}");
    assert!(shown.contains("1 step(s) need you"), "{shown}");
}

#[test]
fn secrets_are_checked_by_name_and_never_shown() {
    let w = World::new();
    w.fake_codex("0.150.0");
    w.codex_logged_in();
    w.project_file(
        "[defaults]\nharness = \"codex\"\n[secrets]\nOPENAI_API_KEY = \"MY_OPENAI\"\n\
         GITHUB_TOKEN = \"@keys/github\"\n",
    );
    let (plan, code) = w.up(&[], &[]);
    assert_eq!(code, Some(1), "{plan}");
    let key = step(&plan, "secret.OPENAI_API_KEY");
    assert_eq!(key["state"], "needs_person");
    assert_eq!(key["detail"], "$MY_OPENAI is not set");
    assert_eq!(step(&plan, "secret.GITHUB_TOKEN")["state"], "needs_person");
    let (plan, code) = w.up(&[], &[("MY_OPENAI", "secret-value-1")]);
    assert_eq!(code, Some(1), "{plan}");
    assert_eq!(step(&plan, "secret.OPENAI_API_KEY")["state"], "ready");
    assert!(!plan.to_string().contains("secret-value-1"));
    fs::create_dir_all(w.repo.join("keys")).unwrap();
    fs::write(w.repo.join("keys/github"), "secret-value-2\n").unwrap();
    let (plan, code) = w.up(&[], &[("MY_OPENAI", "secret-value-1")]);
    assert_eq!(code, Some(0), "{plan}");
    assert!(!plan.to_string().contains("secret-value"));
}

#[test]
fn a_pinned_gateway_with_no_anvil_is_blocked() {
    let w = World::new();
    w.fake_codex("0.150.0");
    w.codex_logged_in();
    w.project_file(
        "[defaults]\nharness = \"codex\"\n[connectors]\ngateway = \"http://127.0.0.1:1/mcp\"\n",
    );
    let (plan, code) = w.up(&[], &[]);
    assert_eq!(code, Some(1), "{plan}");
    let gateway = step(&plan, "connectors");
    assert_eq!(gateway["state"], "blocked");
    assert!(
        gateway["detail"].as_str().unwrap().contains("anvil"),
        "{gateway}"
    );
    assert!(gateway["todo"]
        .as_str()
        .unwrap()
        .contains("docs/connectors.md"));
    assert_eq!(plan["state"], "blocked");
}

#[test]
fn up_refuses_inside_a_harness_and_with_a_remote() {
    let w = World::new();
    w.fake_codex("0.150.0");
    let out = w.by_env(&["up"], &[("BRANCHYARD_BRANCH", "agent/x")]);
    assert_eq!(out.status.code(), Some(4));
    assert!(
        text(&out.stderr).contains("cannot bring the repository up"),
        "{}",
        text(&out.stderr)
    );
    let out = w.by(&["up", "--remote", "http://127.0.0.1:9"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("--remote"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn remote_antigravity_uses_private_home_secret_sources_without_host_agy() {
    let world = World::new();
    world.project_file(
        r#"
[defaults]
harness = "antigravity"
provider = "substrate"
[substrate]
endpoint = "https://control.example"
router = "https://router.example"
atespace = "branchyard"
template = "agy"
key = "/missing-bridge-key"
egress = { mode = "deny" }
[secrets]
GOOGLE_API_KEY = "REMOTE_MODEL_KEY"
"#,
    );
    let (plan, code) = world.up(
        &["--plan"],
        &[("REMOTE_MODEL_KEY", "test-secret-never-print")],
    );
    assert_eq!(code, Some(1)); // Signing key is absent; no network request is needed.
    let steps = plan["steps"].as_array().unwrap();
    assert!(!steps
        .iter()
        .any(|step| step["name"] == "harness.antigravity"));
    let login = steps
        .iter()
        .find(|step| step["name"] == "login.antigravity")
        .unwrap();
    assert_eq!(login["state"], "ready");
    assert!(login["detail"]
        .as_str()
        .unwrap()
        .contains("model authentication unverified"));
    let provider = steps
        .iter()
        .find(|step| step["name"] == "provider")
        .unwrap();
    assert_eq!(provider["state"], "blocked");
    assert!(!plan.to_string().contains("test-secret-never-print"));
    let (plan, _) = world.up(&["--plan"], &[("AGY_TOKEN", "host-token-never-print")]);
    let login = plan["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["name"] == "login.antigravity")
        .unwrap();
    assert_eq!(login["state"], "needs_person");
    assert!(!plan.to_string().contains("host-token-never-print"));
}
