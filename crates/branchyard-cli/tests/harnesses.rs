//! The harness lifecycle through the built `by` (docs/harness-lifecycle.md):
//! detection with fake harness binaries on a temporary `PATH` (logged in,
//! logged out, unknown; missing; a version command that hangs), the
//! cache, detection over `ssh` and on a recipe's machine through
//! `crates/branchyard-recipe/tests/fixtures/fake-ssh`, installs through a
//! fake `npm` that records its calls (policy never, ask and auto, the
//! allowlist, pins, verification, a repository that tries to allow
//! installs), logins (no terminal, a terminal through a pty, over `ssh
//! -t`, an API key), the log, and the router skipping or installing a
//! candidate. Nothing is installed from the network and no real harness
//! runs. Requires `git`, `sh` and `python3`.

#![allow(
    clippy::let_underscore_must_use,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::unwrap_used
)] // tests: a panic is the failure report
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use branchyard_testkit::fake_agent;
use branchyard_testkit::wait;
use serde_json::Value;

const BY: &str = env!("CARGO_BIN_EXE_by");
const FAKE_SSH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../branchyard-recipe/tests/fixtures/fake-ssh"
);

fn which(program: &str) -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|d| d.join(program))
                .find(|p| p.is_file())
        })
        .unwrap_or_else(|| panic!("{program} is needed on PATH"))
}

/// A machine of temporary directories: `bin` (on `PATH`, with the fakes),
/// `home` (with `.local/bin` on `PATH` too), the user configuration, a
/// repository, and `vm-home`, the home of the "remote" machine fake-ssh
/// reaches.
struct World {
    _dir: tempfile::TempDir,
    base: PathBuf,
    bin: PathBuf,
    home: PathBuf,
    user: PathBuf,
    repo: PathBuf,
    vm_home: PathBuf,
}

impl World {
    fn new() -> World {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let (bin, home, repo, vm_home) = (
            base.join("bin"),
            base.join("home"),
            base.join("repo"),
            base.join("vm-home"),
        );
        for d in [
            &bin,
            &home.join(".local/bin"),
            &repo,
            &vm_home.join(".local/bin"),
        ] {
            fs::create_dir_all(d).unwrap();
        }
        // Only what the tests need, so no real harness is found.
        std::os::unix::fs::symlink(which("python3"), bin.join("python3")).unwrap();
        let world = World {
            user: base.join("user/config.toml"),
            _dir: dir,
            base,
            bin,
            home,
            repo,
            vm_home,
        };
        world.script(
            "ssh",
            &format!(
                "#!/bin/sh\nFAKE_SSH_HOME='{}' FAKE_SSH_HOSTS=vm.test FAKE_SSH_LOG='{}' exec \
                 python3 '{FAKE_SSH}' \"$@\"\n",
                world.vm_home.display(),
                world.base.join("ssh.log").display()
            ),
        );
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
            "{}:{}:/usr/bin:/bin",
            self.bin.display(),
            self.home.join(".local/bin").display()
        )
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .env_clear()
            .current_dir(&self.repo)
            .env("PATH", self.path_var())
            .env("HOME", &self.home)
            .env("BRANCHYARD_USER_CONFIG", &self.user)
            .env("BRANCHYARD_TRUST_FILE", self.base.join("trusted.json"))
            .env("BRANCHYARD_SSH", self.bin.join("ssh"))
            // Only $HOME/.local/bin besides PATH: on the "remote", the
            // remote home's.
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

    fn by(&self, args: &[&str]) -> Output {
        self.command(BY)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn by_env(&self, args: &[&str], vars: &[(&str, &str)]) -> Output {
        let mut command = self.command(BY);
        for (k, v) in vars {
            command.env(k, v);
        }
        command.args(args).stdin(Stdio::null()).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let out = self.by(args);
        assert!(
            out.status.success(),
            "by {args:?}\nstdout:\n{}\nstderr:\n{}",
            text(&out.stdout),
            text(&out.stderr)
        );
        out
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_slice(&self.ok(args).stdout).unwrap()
    }

    fn fails(&self, args: &[&str]) -> String {
        let out = self.by(args);
        assert!(
            !out.status.success(),
            "by {args:?} should fail\nstdout:\n{}",
            text(&out.stdout)
        );
        text(&out.stderr)
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

    /// A fake Codex at `dir/codex`: prints `codex-cli VERSION`, and answers
    /// `login status` with what `dir/codex.status` says (default: not
    /// logged in). Its device login writes "Logged in using ChatGPT" there.
    fn fake_codex(&self, dir: &Path, version: &str) -> PathBuf {
        let path = dir.join("codex");
        write_exec(&path, &codex_script(version));
        path
    }

    /// A fake `npm` whose `install -g PACKAGE[@VERSION]` records the call
    /// and installs a fake harness for the package into
    /// `$HOME/.local/bin`; version `9.9.9` is "installed" without changing
    /// anything, so verification fails.
    fn fake_npm(&self) {
        let agent = fake_agent!().display().to_string();
        let codex = branchyard_recipe::quote(&codex_script("@VERSION@"));
        self.script(
            "npm",
            &format!(
                r#"#!/bin/sh
echo "npm $*" >> "$FAKE_LOG"
[ "$1" = install ] && [ "$2" = -g ] || exit 2
package=$3
case $package in
  @*/*@*) name=${{package%@*}}; version=${{package##*@}};;
  @*) name=$package; version=;;
  *@*) name=${{package%@*}}; version=${{package##*@}};;
  *) name=$package; version=;;
esac
[ "$version" = 9.9.9 ] && exit 0
mkdir -p "$HOME/.local/bin"
case $name in
  @openai/codex)
    printf '%s' {codex} | sed "s/@VERSION@/${{version:-0.150.0}}/" > "$HOME/.local/bin/codex"
    chmod +x "$HOME/.local/bin/codex";;
  @qwen-code/qwen-code)
    printf '#!/bin/sh\nif [ "$1" = --version ]; then echo 0.5.0; exit 0; fi\nexec %s\n' '{agent}' > "$HOME/.local/bin/qwen"
    chmod +x "$HOME/.local/bin/qwen";;
  *) echo "no such package $name" >&2; exit 1;;
esac
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
  "login --device-auth")
    echo "Open https://auth.example/device and enter the code WXYZ-1234"
    echo "Logged in using ChatGPT" > "$status";;
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

fn harness<'a>(inventory: &'a Value, id: &str) -> Option<&'a Value> {
    inventory["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["id"] == id)
}

#[test]
fn detection_reads_versions_and_logins_without_reading_secrets() {
    let w = World::new();
    // Codex: its own status command says logged in (verified).
    w.fake_codex(&w.bin, "0.157.1");
    fs::write(w.bin.join("codex.status"), "Logged in using ChatGPT\n").unwrap();
    // Claude Code: a credential file (likely logged in).
    w.script("claude", "#!/bin/sh\necho '2.1.283 (Claude Code)'\n");
    fs::create_dir_all(w.home.join(".claude")).unwrap();
    fs::write(w.home.join(".claude/.credentials.json"), "{}").unwrap();
    // Gemini CLI: a key variable, by name only (likely logged in).
    w.script("gemini", "#!/bin/sh\necho 0.9.1\n");
    // Grok: none of its credential files or variables (likely out).
    w.script("grok", "#!/bin/sh\necho 'grok 1.4.0'\n");
    // Goose: nothing known about its login; a version without a dot.
    w.script("goose", "#!/bin/sh\necho 'goose build 7'\n");
    // Qwen Code: a version command that hangs.
    w.script("qwen", "#!/bin/sh\nexec sleep 30\n");
    let secret = "sekrit-value-123";
    let out = w.by_env(&["harnesses", "--json"], &[("GOOGLE_API_KEY", secret)]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!text(&out.stdout).contains(secret));
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let ids: Vec<&str> = inventory["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "claude-code",
            "codex",
            "gemini-cli",
            "goose",
            "qwen-code",
            "grok-build"
        ]
    );
    assert!(inventory["checked"].as_array().unwrap().len() > 40);
    assert!(harness(&inventory, "opencode").is_none());

    let codex = harness(&inventory, "codex").unwrap();
    assert_eq!(codex["version"], "0.157.1");
    assert_eq!(codex["login"]["state"], "logged_in");
    assert_eq!(codex["login"]["evidence"], "verified");
    assert!(codex["login"]["detail"]
        .as_str()
        .unwrap()
        .contains("codex login status: Logged in using ChatGPT"));
    let claude = harness(&inventory, "claude-code").unwrap();
    assert_eq!(claude["version"], "2.1.283");
    assert_eq!(claude["login"]["state"], "logged_in");
    assert_eq!(claude["login"]["evidence"], "likely");
    let gemini = harness(&inventory, "gemini-cli").unwrap();
    assert_eq!(gemini["login"]["evidence"], "likely");
    assert!(gemini["login"]["detail"]
        .as_str()
        .unwrap()
        .contains("GOOGLE_API_KEY is set"));
    let grok = harness(&inventory, "grok-build").unwrap();
    assert_eq!(grok["version"], "1.4.0");
    assert_eq!(grok["login"]["state"], "logged_out");
    assert_eq!(grok["login"]["evidence"], "likely");
    assert_eq!(
        grok["login"]["detail"],
        "no ~/.grok/auth.json and no XAI_API_KEY"
    );
    let goose = harness(&inventory, "goose").unwrap();
    assert_eq!(goose["login"]["state"], "unknown");
    assert!(goose.get("version").is_none());
    assert_eq!(
        goose["version_note"],
        "its version command printed no version"
    );
    let qwen = harness(&inventory, "qwen-code").unwrap();
    assert_eq!(qwen["version_note"], "its version command timed out");
    assert_eq!(qwen["on_path"], true);

    // The table says the same, and why a harness cannot run.
    let out = w.ok(&["harnesses", "--refresh"]);
    let table = text(&out.stdout);
    assert!(table.contains("codex"), "{table}");
    assert!(table.contains("logged in (verified)"), "{table}");
    assert!(table.contains("logged out (likely)"), "{table}");
    assert!(
        table.contains("grok-build: cannot run: grok-build is likely logged out"),
        "{table}"
    );
    assert!(
        table.contains("qwen-code: no version: its version command timed out"),
        "{table}"
    );
    assert!(table.contains("6 of the"), "{table}");

    // Off PATH, in a known install directory: found, but cannot run.
    let other = w.base.join("other-home");
    fs::create_dir_all(other.join(".local/bin")).unwrap();
    write_exec(&other.join(".local/bin/pi"), "#!/bin/sh\necho 0.87.1\n");
    let out = w.by_env(
        &["harnesses", "--json", "--refresh"],
        &[(
            "BRANCHYARD_HARNESS_DIRS",
            &other.join(".local/bin").display().to_string(),
        )],
    );
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let pi = harness(&inventory, "pi").unwrap();
    assert_eq!(pi["on_path"], false);
    assert_eq!(pi["version"], "0.87.1");
}

#[test]
fn the_inventory_is_cached_for_a_minute_and_refreshed_on_request() {
    let w = World::new();
    w.fake_codex(&w.bin, "0.157.1");
    let first = w.json(&["harnesses", "--json"]);
    assert_eq!(w.calls("codex --version").len(), 1);
    let second = w.json(&["harnesses", "--json"]);
    assert_eq!(
        w.calls("codex --version").len(),
        1,
        "the second read is cached"
    );
    assert_eq!(first["detected_at_ms"], second["detected_at_ms"]);
    w.json(&["harnesses", "--json", "--refresh"]);
    assert_eq!(w.calls("codex --version").len(), 2);
    // A different PATH is a different machine as far as the cache goes.
    let out = w.by_env(
        &["harnesses", "--json"],
        &[("PATH", &format!("{}:/usr/bin:/bin", w.base.display()))],
    );
    assert!(out.status.success());
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(harness(&inventory, "codex").is_none());
}

#[test]
fn detection_runs_over_ssh_and_on_a_recipes_machine() {
    let w = World::new();
    // Only the "remote" has Codex, in its home's .local/bin.
    w.fake_codex(&w.vm_home.join(".local/bin"), "0.157.1");
    let here = w.json(&["harnesses", "--json"]);
    assert!(harness(&here, "codex").is_none());
    let there = w.json(&["harnesses", "--json", "--on", "ssh://dev@vm.test"]);
    let codex = harness(&there, "codex").unwrap();
    assert_eq!(codex["version"], "0.157.1");
    assert_eq!(codex["on_path"], false);
    assert!(codex["path"]
        .as_str()
        .unwrap()
        .starts_with(&w.vm_home.display().to_string()));
    let log = fs::read_to_string(w.base.join("ssh.log")).unwrap();
    assert!(
        log.contains("BatchMode=yes") && log.contains("\"-l\", \"dev\""),
        "{log}"
    );
    let unknown = w.fails(&["harnesses", "--on", "ssh://elsewhere.test"]);
    assert!(unknown.contains("on ssh://elsewhere.test"), "{unknown}");
    assert!(w
        .fails(&["harnesses", "--on", "ftp://x"])
        .contains("is not ssh://"));

    // A recipe's machine: made, looked at over its transport, destroyed.
    fs::create_dir_all(w.repo.join("vm")).unwrap();
    fs::write(
        w.repo.join("branchyard.toml"),
        "[recipes.devbox]\ncreate = \"./vm/create.sh\"\ndestroy = \"./vm/destroy.sh\"\n",
    )
    .unwrap();
    write_exec(
        &w.repo.join("vm/create.sh"),
        "#!/bin/sh\nprintf '{\"schemaVersion\":1,\"connection\":{\"type\":\"ssh\",\"target\":\
         {\"host\":\"vm.test\",\"username\":\"dev\"},\"projectRoot\":\"/tmp\"}}\\n'\n",
    );
    write_exec(
        &w.repo.join("vm/destroy.sh"),
        "#!/bin/sh\ncat > \"$BRANCHYARD_ROOT/destroyed-$BRANCHYARD_RECIPE_INSTANCE\"\n",
    );
    // Untrusted, with no terminal: refused before anything runs.
    let refused = w.fails(&["harnesses", "--on", "recipe:devbox"]);
    assert!(refused.contains("by recipe trust devbox"), "{refused}");
    w.ok(&["recipe", "trust", "devbox"]);
    let machine = w.json(&["harnesses", "--json", "--on", "recipe:devbox"]);
    assert_eq!(harness(&machine, "codex").unwrap()["version"], "0.157.1");
    let destroyed: Vec<_> = fs::read_dir(&w.repo)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("destroyed-"))
        .collect();
    assert_eq!(destroyed.len(), 1, "the machine is destroyed afterwards");
    // Installing on a recipe's machine would not last: refused with the
    // command to put in its create script.
    let install = w.fails(&["harnesses", "install", "codex", "--on", "recipe:devbox"]);
    assert!(
        install.contains("create script: npm install -g @openai/codex"),
        "{install}"
    );
}

#[test]
fn installs_follow_the_policy_and_are_verified_and_logged() {
    let w = World::new();
    w.fake_npm();
    // Unset policy and no terminal: never.
    let never = w.fails(&["harnesses", "install", "codex"]);
    assert!(
        never.contains("[harnesses] install is \"never\" here"),
        "{never}"
    );
    assert!(w.calls("npm ").is_empty());
    // A repository cannot allow installs.
    fs::write(
        w.repo.join("branchyard.toml"),
        "[harnesses]\ninstall = \"auto\"\n",
    )
    .unwrap();
    let repo = w.fails(&["harnesses", "install", "codex", "--yes"]);
    assert!(
        repo.contains(
            "a repository's branchyard.toml may only set [harnesses] install = \"never\""
        ),
        "{repo}"
    );
    fs::write(
        w.repo.join("branchyard.toml"),
        "[harnesses]\ninstall = \"never\"\n",
    )
    .unwrap();
    w.user_config("[harnesses]\ninstall = \"ask\"\n");
    // The repository's "never" wins over the user's "ask".
    assert!(w
        .fails(&["harnesses", "install", "codex", "--yes"])
        .contains("\"never\""));
    fs::remove_file(w.repo.join("branchyard.toml")).unwrap();
    // "ask" without a terminal needs --yes.
    let ask = w.fails(&["harnesses", "install", "codex"]);
    assert!(ask.contains("pass --yes"), "{ask}");
    assert!(w.calls("npm ").is_empty());
    // With it: the catalog's npm command, pinned to the profile's version.
    let out = w.ok(&["harnesses", "install", "codex", "--yes"]);
    assert!(
        text(&out.stdout).contains("installed codex 0.157.1 on local"),
        "{}",
        text(&out.stdout)
    );
    assert!(text(&out.stderr).contains("npm install -g @openai/codex@0.157.1"));
    assert_eq!(w.calls("npm "), ["npm install -g @openai/codex@0.157.1"]);
    // Installed already: nothing runs.
    let again = w.ok(&["harnesses", "install", "codex", "--yes"]);
    assert!(text(&again.stdout).contains("already installed"));
    assert_eq!(w.calls("npm ").len(), 1);
    // Update to a pinned version, checked after.
    let out = w.json(&[
        "harnesses",
        "update",
        "codex",
        "--version",
        "0.200.0",
        "--yes",
        "--json",
    ]);
    assert_eq!(out["verified"], true, "{out}");
    assert_eq!(out["plan"]["pinned"], "0.200.0");
    assert_eq!(out["before"], "0.157.1");
    assert_eq!(out["after"]["version"], "0.200.0");
    // An install that does not produce the pinned version fails to verify.
    let bad = w.fails(&[
        "harnesses",
        "update",
        "codex",
        "--version",
        "9.9.9",
        "--yes",
    ]);
    assert!(bad.contains("not the pinned 9.9.9"), "{bad}");
    // "auto" with an allowlist that does not name it.
    w.user_config("[harnesses]\ninstall = \"auto\"\nallow = [\"goose\"]\n");
    let allow = w.fails(&["harnesses", "update", "codex"]);
    assert!(
        allow.contains("codex is not in [harnesses] allow (goose)"),
        "{allow}"
    );
    // Never from a harness's own `by`.
    let inside = w.by_env(
        &["harnesses", "install", "goose"],
        &[("BRANCHYARD_BRANCH", "b")],
    );
    assert!(!inside.status.success());
    assert!(text(&inside.stderr).contains("cannot install harnesses"));
    // A harness without an install command, and one Branchyard does not know.
    w.user_config("[harnesses]\ninstall = \"auto\"\n");
    assert!(w
        .fails(&["harnesses", "install", "gemini-cli"])
        .contains("no install command"));
    assert!(w
        .fails(&["harnesses", "install", "nope"])
        .contains("not a harness"));
    // Every attempt is in the log.
    let log = w.json(&["harnesses", "log", "--json"]);
    let outcomes: Vec<(&str, &str)> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["action"].as_str().unwrap(),
                e["outcome"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        outcomes,
        [
            ("install", "refused"),
            ("install", "refused"),
            ("install", "refused"),
            ("install", "verified"),
            ("update", "verified"),
            ("update", "failed"),
            ("update", "refused"),
        ]
    );
    let verified = &log[3];
    assert_eq!(verified["command"], "npm install -g @openai/codex@0.157.1");
    assert_eq!(verified["version_after"], "0.157.1");
    assert_eq!(verified["on"], "local");
    let table = text(&w.ok(&["harnesses", "log"]).stdout);
    assert!(table.contains("0.157.1 → 0.200.0"), "{table}");
}

#[test]
fn installs_and_logins_reach_another_machine_over_ssh() {
    let w = World::new();
    w.fake_npm();
    w.user_config("[harnesses]\ninstall = \"auto\"\n");
    let out = w.json(&[
        "harnesses",
        "install",
        "codex",
        "--on",
        "ssh://dev@vm.test",
        "--json",
    ]);
    assert_eq!(out["verified"], true, "{out}");
    assert_eq!(out["on"], "ssh://dev@vm.test");
    assert!(w.vm_home.join(".local/bin/codex").is_file());
    assert!(!w.home.join(".local/bin/codex").exists());
    // No terminal: the login is reported, with where to run it.
    let out = w.ok(&["harnesses", "login", "codex", "--on", "ssh://dev@vm.test"]);
    // Codex is off PATH there, so the login names the executable found.
    let said = text(&out.stdout);
    assert!(said.contains("run `ssh -t dev@vm.test /"), "{said}");
    assert!(
        said.contains(".local/bin/codex login --device-auth`"),
        "{said}"
    );
    // With one, through `ssh -t`: the device code reaches the person, and
    // the login is verified afterwards.
    let out = in_pty(
        &w,
        &["harnesses", "login", "codex", "--on", "ssh://dev@vm.test"],
    );
    assert!(out.contains("WXYZ-1234"), "{out}");
    assert!(out.contains("logged in (verified"), "{out}");
    let log = fs::read_to_string(w.base.join("ssh.log")).unwrap();
    assert!(log.contains("\"-t\""), "{log}");
}

/// `by ARGS` with a terminal, through Python's pty module; its output.
fn in_pty(w: &World, args: &[&str]) -> String {
    let out = w
        .command("python3")
        .arg("-c")
        .arg(
            "import os, pty, sys\n\
             status = pty.spawn(sys.argv[1:])\n\
             sys.exit(os.waitstatus_to_exitcode(status))",
        )
        .arg(BY)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let all = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(out.status.success(), "{all}");
    all
}

#[test]
fn login_runs_the_harness_own_flow_or_stores_a_key_and_says_what_to_run() {
    let w = World::new();
    w.fake_codex(&w.bin, "0.157.1");
    // No terminal: say what to run.
    let out = w.ok(&["harnesses", "login", "codex"]);
    assert!(
        text(&out.stdout).contains("no terminal here; run `codex login --device-auth`"),
        "{}",
        text(&out.stdout)
    );
    assert!(w.calls("codex login --device-auth").is_empty());
    // A terminal: the harness's own flow, passed through.
    let out = in_pty(&w, &["harnesses", "login", "codex"]);
    assert!(out.contains("https://auth.example/device"), "{out}");
    assert!(out.contains("codex on local: logged in (verified"), "{out}");
    assert_eq!(w.calls("codex login --device-auth").len(), 1);
    let inventory = w.json(&["harnesses", "--json"]);
    assert_eq!(
        harness(&inventory, "codex").unwrap()["login"]["state"],
        "logged_in"
    );
    // An API key from standard input: a 0600 file named in [secrets],
    // never echoed, never in the repository.
    let key = "sk-ant-test-0123456789";
    let mut child = w
        .command(BY)
        .args(["harnesses", "login", "claude-code", "--api-key"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(key.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!text(&out.stdout).contains(key) && !text(&out.stderr).contains(key));
    let file = w.user.parent().unwrap().join("secrets/ANTHROPIC_API_KEY");
    assert_eq!(fs::read_to_string(&file).unwrap(), key);
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let config = fs::read_to_string(&w.user).unwrap();
    assert!(
        config.contains(&format!("ANTHROPIC_API_KEY = \"@{}\"", file.display())),
        "{config}"
    );
    assert!(!config.contains(key));
    w.ok(&["config", "validate"]);
    let tracked = Command::new("git")
        .args(["status", "--porcelain", "--ignored"])
        .current_dir(&w.repo)
        .output()
        .unwrap();
    assert!(!text(&tracked.stdout).contains("secrets"));
    // Without a key, or for a harness with no key variable: refused.
    assert!(w
        .fails(&["harnesses", "login", "claude-code", "--api-key"])
        .contains("no key on standard input"));
    assert!(w
        .fails(&["harnesses", "login", "goose", "--api-key"])
        .contains("reads no API key variable"));
    let log = w.json(&["harnesses", "log", "--json"]);
    let outcomes: Vec<&str> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(outcomes, ["reported", "ran", "stored_key"]);
    assert!(!serde_json::to_string(&log).unwrap().contains(key));
}

#[test]
fn the_router_skips_what_cannot_run_and_installs_on_demand_under_auto() {
    let w = World::new();
    w.fake_npm();
    // Codex installed but logged out (verified by its status command).
    w.fake_codex(&w.bin, "0.157.1");
    let agent = fake_agent!().display().to_string();
    let fleet = format!(
        "[fleet.default]\ncandidates = [\n  {{ harness = \"codex\" }},\n  {{ harness = \
         \"qwen-code\" }},\n  {{ harness = \"gemini-cli\", command = \"{agent}\" }},\n]\n"
    );
    fs::write(w.repo.join("branchyard.toml"), &fleet).unwrap();
    let route = w.json(&["fleet", "route", "fix the bug", "--seed", "1", "--json"]);
    assert_eq!(
        route["picks"][0]["candidate"]["harness"], "gemini-cli",
        "{route}"
    );
    let reasons: Vec<(String, String)> = route["excluded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["candidate"]["harness"].as_str().unwrap().to_owned(),
                e["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(reasons.len(), 2, "{reasons:?}");
    assert_eq!(reasons[0].0, "codex");
    assert!(
        reasons[0].1.contains("codex is verified logged out"),
        "{reasons:?}"
    );
    assert_eq!(reasons[1].0, "qwen-code");
    assert!(
        reasons[1].1.contains(
            "not installed on this machine; install it with `by harnesses install qwen-code`"
        ),
        "{reasons:?}"
    );
    // Under "auto", a preview says it would install; a routed run does.
    w.user_config("[harnesses]\ninstall = \"auto\"\n");
    let route = w.json(&["fleet", "route", "fix the bug", "--seed", "1", "--json"]);
    let preview = route["excluded"].to_string();
    assert!(
        preview.contains("a routed run would install it first"),
        "{preview}"
    );
    assert!(w.calls("npm ").is_empty());
    fs::write(
        w.repo.join("branchyard.toml"),
        "[fleet.default]\ncandidates = [{ harness = \"qwen-code\" }]\n",
    )
    .unwrap();
    let out = w.ok(&[
        "run",
        "Fix it WRITE fixed.txt=1",
        "-n",
        "on-demand",
        "--seed",
        "1",
        "--yes",
    ]);
    let err = text(&out.stderr);
    assert!(err.contains("installing qwen-code on demand"), "{err}");
    assert!(err.contains("installed qwen-code 0.5.0"), "{err}");
    assert_eq!(w.calls("npm "), ["npm install -g @qwen-code/qwen-code"]);
    let show = w.json(&["show", "on-demand", "--json"]);
    assert_eq!(show["status"]["state"], "ready", "{show}");
    let log = w.json(&["harnesses", "log", "--json"]);
    assert_eq!(log[0]["by"], "router");
    assert_eq!(log[0]["outcome"], "verified");
    assert_eq!(log[0]["version_after"], "0.5.0");
}

#[test]
fn a_server_shows_its_workers_harnesses_and_says_where_to_log_in() {
    use std::io::{BufRead, BufReader};
    let w = World::new();
    w.fake_codex(&w.bin, "0.157.1");
    let data = w.base.join("data");
    let mut child = w
        .command(BY)
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--quiet",
            "--shutdown-grace",
            "5",
        ])
        .arg("--data-dir")
        .arg(&data)
        .arg("--repo")
        .arg(format!("app={}", w.repo.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = line
        .trim()
        .strip_prefix("listening on ")
        .unwrap()
        .to_owned();
    let token = data.join("token").display().to_string();
    let remote = |args: &[&str]| {
        let mut all = vec!["--remote", url.as_str(), "--token-file", token.as_str()];
        all.extend_from_slice(args);
        w.by(&all)
    };
    // The worker detects in the background and advertises with its beats.
    let report = wait::until("the worker to advertise its inventory", || {
        let out = remote(&["harnesses", "--json"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        let report: Value = serde_json::from_slice(&out.stdout).unwrap();
        if report["workers"][0]["inventory"].is_object() {
            Ok(report)
        } else {
            Err(report)
        }
    });
    let worker = &report["workers"][0];
    assert_eq!(worker["this"], true);
    assert_eq!(worker["repos"], serde_json::json!(["app"]));
    // Logged out (verified), so no harness:codex label.
    assert_eq!(worker["labels"], serde_json::json!([]));
    let codex = harness(&worker["inventory"], "codex").unwrap();
    assert_eq!(codex["login"]["state"], "logged_out");
    let table = text(&remote(&["harnesses"]).stdout);
    assert!(table.contains("(this server), serving app"), "{table}");
    assert!(table.contains("codex: cannot run"), "{table}");
    // A worker has no terminal: login says what to run there.
    let said = text(&remote(&["harnesses", "login", "codex"]).stdout);
    assert!(
        said.contains("run `codex login --device-auth` there, in a terminal"),
        "{said}"
    );
    // Workers never install.
    let out = remote(&["harnesses", "install", "codex"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("never install harnesses themselves"));
    assert!(!remote(&["harnesses", "--on", "ssh://vm.test"])
        .status
        .success());
    let _ = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
    let _ = child.wait();
}
