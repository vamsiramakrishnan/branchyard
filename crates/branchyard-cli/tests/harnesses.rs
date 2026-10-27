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

#![allow(clippy::panic, clippy::unwrap_in_result, clippy::unwrap_used)] // tests: a panic is the failure report
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
        branchyard_testkit::hermetic(command.env_clear())
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

    /// `by ARGS` with `input` on standard input, as a key is piped in.
    fn by_input(&self, args: &[&str], input: &str) -> Output {
        use std::io::Write;
        let mut child = self
            .command(BY)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
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

    /// A fake `gemini` that prints a version and otherwise speaks ACP, as
    /// the fake agent: a harness that is installed, with no login file.
    fn fake_gemini(&self) {
        let agent = fake_agent!().display().to_string();
        self.script(
            "gemini",
            &format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo 0.65.0; exit 0; fi\nexec {agent}\n"),
        );
    }

    /// The names of the branches here.
    fn branches(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .json(&["ls", "--json"])
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["name"].as_str().unwrap().to_owned())
            .collect();
        names.sort();
        names
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

/// `by harnesses --all` for DeepSeek Harness 0.2.0-rc.2 (`dsh`), Gemini CLI
/// 0.63.0 and Antigravity CLI 1.3.3, whose entries were corrected by
/// running them: how to install each, sign in, and give it a key.
#[test]
fn the_full_listing_says_how_to_install_log_in_and_key_the_harnesses_that_were_run() {
    let w = World::new();
    let list = w.json(&["harnesses", "--all", "--json"]);
    let row = |id: &str| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == id)
            .unwrap_or_else(|| panic!("{id} is listed"))
    };
    let dsh = row("deepseek-harness");
    assert_eq!(dsh["binaries"], serde_json::json!(["dsh"]));
    assert_eq!(
        dsh["install"],
        serde_json::json!(["npm install -g @deepseek-ai/dsh"])
    );
    assert_eq!(dsh["auth_env"], serde_json::json!(["DEEPSEEK_API_KEY"]));
    assert_eq!(dsh["drivable"], true);
    let gemini = row("gemini-cli");
    assert_eq!(
        gemini["install"],
        serde_json::json!(["npm install -g @google/gemini-cli"])
    );
    assert_eq!(gemini["login"], "gemini");
    assert_eq!(
        gemini["auth_env"],
        serde_json::json!(["GEMINI_API_KEY", "GOOGLE_API_KEY"])
    );
    let agy = row("antigravity");
    assert_eq!(agy["login"], "agy");
    assert_eq!(agy["auth_env"], serde_json::json!(["GEMINI_API_KEY"]));

    // The table shows them in the install, login and key columns.
    let table = text(&w.ok(&["harnesses", "--all"]).stdout);
    let line = |id: &str| {
        table
            .lines()
            .find(|l| l.starts_with(id))
            .unwrap_or_else(|| panic!("{id} is in the table:\n{table}"))
    };
    for (id, wants) in [
        (
            "deepseek-harness",
            &["npm install -g @deepseek-ai/dsh", "DEEPSEEK_API_KEY"][..],
        ),
        (
            "gemini-cli",
            &[
                "npm install -g @google/gemini-cli",
                "gemini",
                "GEMINI_API_KEY, GOOGLE_API_KEY",
            ][..],
        ),
        (
            "antigravity",
            &["antigravity.google/cli/install.sh", "agy", "GEMINI_API_KEY"][..],
        ),
    ] {
        for want in wants {
            assert!(line(id).contains(want), "{id} lacks {want}:\n{}", line(id));
        }
    }
}

#[test]
fn detection_reads_versions_and_logins_without_reading_secrets() {
    let w = World::new();
    // Codex: its own status command says logged in (verified).
    w.fake_codex(&w.bin, "0.164.0");
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
    assert_eq!(codex["version"], "0.164.0");
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
    assert_eq!(qwen["version_note"], "slow to start (took >1 s)");
    assert_eq!(qwen["on_path"], true);

    // The table says the same, and why a harness cannot run.
    let out = w.by_env(&["harnesses", "--refresh"], &[("GOOGLE_API_KEY", secret)]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let table = text(&out.stdout);
    assert!(table.contains("codex"), "{table}");
    // Words for what is known: `logged in` only from the harness's own
    // status command, a key by name, a file found, and an unconfirmed login
    // is not called logged out.
    let cell = |id: &str| -> String {
        table
            .lines()
            .find(|l| l.starts_with(id))
            .unwrap_or_else(|| panic!("{id} is in the table:\n{table}"))
            .to_owned()
    };
    assert!(cell("codex").contains("logged in "), "{table}");
    assert!(cell("claude-code").contains("credentials found"), "{table}");
    assert!(
        cell("gemini-cli").contains("key set (GOOGLE_API_KEY)"),
        "{table}"
    );
    assert!(cell("grok-build").contains("login not verified"), "{table}");
    assert!(cell("goose").contains("unknown"), "{table}");
    assert!(
        !table.contains("(likely)") && !table.contains("(verified)"),
        "{table}"
    );
    assert!(
        table.contains("grok-build: login not verified: no ~/.grok/auth.json and no XAI_API_KEY"),
        "{table}"
    );
    assert!(!table.contains("cannot run: grok-build"), "{table}");
    assert!(cell("qwen-code").contains("slow to start"), "{table}");
    assert!(
        table.contains("qwen-code: version: slow to start (took >1 s); `by harnesses --refresh`"),
        "{table}"
    );
    assert!(table.contains("BRANCHYARD_HARNESS_TIMEOUT"), "{table}");
    // Three honest levels of a live run, and the installed version named
    // beside the one that was run.
    assert!(cell("codex").contains("live 9/9"), "{table}");
    assert!(cell("goose").contains("no live run"), "{table}");
    assert!(
        !table.contains("codex: installed"),
        "no drift for codex: {table}"
    );
    assert!(table.contains("6 of the"), "{table}");
    // This machine is called "this machine", not its hostname.
    assert!(table.contains("are installed on this machine."), "{table}");

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
    // The listing says when the cache answered, and how to ask again.
    let cached = text(&w.ok(&["harnesses"]).stdout);
    assert_eq!(w.calls("codex --version").len(), 1);
    let note = cached
        .lines()
        .find(|l| l.contains("are installed on this machine (cached "))
        .unwrap_or_else(|| panic!("no cached note:\n{cached}"));
    assert!(note.contains("s ago; --refresh asks again). "), "{note}");
    w.json(&["harnesses", "--json", "--refresh"]);
    assert_eq!(w.calls("codex --version").len(), 2);
    let fresh = text(&w.ok(&["harnesses", "--refresh"]).stdout);
    assert_eq!(w.calls("codex --version").len(), 3);
    assert!(!fresh.contains("(cached "), "{fresh}");
    // `by doctor` and `by setup` read the same cache, and `--refresh` asks
    // the harnesses again there too.
    // (Whether they pass is theirs to say: Codex is logged out here.)
    w.by(&["doctor"]);
    assert_eq!(w.calls("codex --version").len(), 3, "doctor read the cache");
    w.by(&["doctor", "--refresh"]);
    assert_eq!(w.calls("codex --version").len(), 4, "doctor --refresh");
    w.by(&["setup", "--dry-run"]);
    assert_eq!(w.calls("codex --version").len(), 4, "setup read the cache");
    w.by(&["setup", "--dry-run", "--refresh"]);
    assert_eq!(w.calls("codex --version").len(), 5, "setup --refresh");
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
    // It names the file to edit and the key to set, and what to run after.
    assert!(
        never.contains(&format!(
            "put this in {}:\n  [harnesses]\n  install = \"ask\"\nThen run `by harnesses install \
             codex --yes`",
            w.user.display()
        )),
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
        text(&out.stdout).contains("installed codex 0.157.1 on this machine"),
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
    assert!(
        allow.contains(&format!(
            "add \"codex\" to `allow` under [harnesses] in {}",
            w.user.display()
        )),
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
        .fails(&["harnesses", "install", "trae"])
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
    // No terminal: the login is not done (exit 3), with where to run it.
    let out = w.by(&["harnesses", "login", "codex", "--on", "ssh://dev@vm.test"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    // Codex is off PATH there, so the login names the executable found.
    let said = text(&out.stderr);
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

/// A fake harness that answers `--version` only once its peer has started:
/// a harness asked alone never sees the peer and prints `-alone`.
fn peer_script(me: &str, other: &str) -> String {
    format!(
        r#"#!/bin/sh
[ "$1" = --version ] || exit 3
touch "$FAKE_PEERS/{me}"
i=0
while [ ! -f "$FAKE_PEERS/{other}" ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done
if [ -f "$FAKE_PEERS/{other}" ]; then echo "{me} 1.2.3-together"; else echo "{me} 1.2.3-alone"; fi
"#
    )
}

#[test]
fn every_installed_harness_is_asked_its_version_at_once() {
    let w = World::new();
    w.script("goose", &peer_script("goose", "pi"));
    w.script("pi", &peer_script("pi", "goose"));
    let peers = w.base.join("peers");
    fs::create_dir_all(&peers).unwrap();
    // Asked one after the other, the first would wait for a peer that has
    // not started and print `-alone`; asked together, both see each other.
    let out = w.by_env(
        &["harnesses", "--json"],
        &[
            ("FAKE_PEERS", &peers.display().to_string()),
            ("BRANCHYARD_HARNESS_TIMEOUT", "60"),
        ],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        harness(&inventory, "goose").unwrap()["version"],
        "1.2.3-together"
    );
    assert_eq!(
        harness(&inventory, "pi").unwrap()["version"],
        "1.2.3-together"
    );
}

#[test]
fn a_cli_that_takes_four_seconds_to_start_still_shows_its_version() {
    let w = World::new();
    // A cold Node harness: four seconds to print a version.
    w.script("goose", "#!/bin/sh\nsleep 4\necho 'goose 1.9.0'\n");
    // The default bound, not the one the test world sets for hanging
    // commands (the bound is part of what the cache is keyed by).
    let by_default = |args: &[&str]| -> Output {
        let mut command = w.command(BY);
        command
            .env_remove("BRANCHYARD_HARNESS_TIMEOUT")
            .args(args)
            .stdin(Stdio::null());
        let out = command.output().unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        out
    };
    let inventory: Value =
        serde_json::from_slice(&by_default(&["harnesses", "--json"]).stdout).unwrap();
    let goose = harness(&inventory, "goose").unwrap();
    assert_eq!(goose["version"], "1.9.0", "{goose}");
    assert!(goose.get("version_note").is_none());
    // Kept: the second call is read from the cache, not asked again.
    let again: Value =
        serde_json::from_slice(&by_default(&["harnesses", "--json"]).stdout).unwrap();
    assert_eq!(again["detected_at_ms"], inventory["detected_at_ms"]);
    let table = text(&by_default(&["harnesses"]).stdout);
    assert!(table.contains("1.9.0") && !table.contains(" ? "), "{table}");
}

#[test]
fn a_harness_that_answered_once_is_not_shown_as_unknown_when_it_is_slow_later() {
    let w = World::new();
    let pi = w.script("pi", "#!/bin/sh\necho 'pi 0.87.1'\n");
    let first = w.json(&["harnesses", "--json"]);
    assert_eq!(harness(&first, "pi").unwrap()["version"], "0.87.1");
    // Now it hangs past the bound (one second in this world).
    write_exec(&pi, "#!/bin/sh\nexec sleep 30\n");
    let slow = w.json(&["harnesses", "--json", "--refresh"]);
    let pi = harness(&slow, "pi").unwrap();
    assert_eq!(pi["version"], "0.87.1", "kept from the last answer: {pi}");
    let note = pi["version_note"].as_str().unwrap();
    assert!(
        note.starts_with("slow to start (took >1 s)") && note.contains("last time it answered"),
        "{note}"
    );
    let table = text(&w.ok(&["harnesses"]).stdout);
    assert!(
        table.contains("0.87.1") && !table.contains(" ? "),
        "{table}"
    );
    assert!(table.contains("pi: version: slow to start"), "{table}");
    // One that never answered says why and what to do, never a bare `?`.
    w.script("qwen", "#!/bin/sh\nexec sleep 30\n");
    let out = text(&w.ok(&["harnesses", "--refresh"]).stdout);
    let qwen = out.lines().find(|l| l.starts_with("qwen-code")).unwrap();
    assert!(
        qwen.contains("slow to start") && !qwen.contains(" ? "),
        "{out}"
    );
    assert!(out.contains("`by harnesses --refresh` asks again"), "{out}");
}

#[test]
fn an_unreadable_login_says_what_would_tell() {
    let w = World::new();
    w.script("pi", "#!/bin/sh\necho 'pi 0.87.1'\n");
    w.script("qwen", "#!/bin/sh\necho '0.26.0'\n");
    let out = text(&w.ok(&["harnesses"]).stdout);
    // Qwen Code records no key variable: only signing in inside it tells.
    assert!(
        out.contains("qwen-code: Branchyard cannot read qwen's login")
            && out.contains("sign in inside qwen itself"),
        "{out}"
    );
    // Pi's provider keys are known (`ZAI_API_KEY` and the rest), so a
    // missing one is named, not "sign in inside pi itself".
    assert!(
        out.contains("pi: Branchyard cannot read pi's login; a key in ANTHROPIC_API_KEY")
            && out.contains("ZAI_API_KEY would tell"),
        "{out}"
    );
    assert!(!out.contains("knows no way to tell"), "{out}");
}

#[test]
fn a_pi_provider_key_in_the_environment_is_a_login() {
    let w = World::new();
    w.script("pi", "#!/bin/sh\necho 'pi 0.87.1'\n");
    let out = w.by_env(
        &["harnesses", "--json"],
        &[("ZAI_API_KEY", "not-a-real-key")],
    );
    let keyed: Value = serde_json::from_slice(&out.stdout).unwrap();
    let pi = harness(&keyed, "pi").unwrap();
    assert_eq!(pi["login"]["state"], "logged_in", "{pi}");
    assert_eq!(pi["login"]["key"], "ZAI_API_KEY", "{pi}");
}

#[test]
fn a_pi_older_than_the_driver_can_follow_is_refused_before_a_branch_exists() {
    let w = World::new();
    w.script("pi", "#!/bin/sh\necho '0.73.1'\n");
    let listed = text(&w.ok(&["harnesses"]).stdout);
    assert!(
        listed.contains("0.73.1 (too old)")
            && listed.contains("pi: cannot run: 0.73.1 is older than 0.80.4")
            && listed.contains("npm install -g @earendil-works/pi-coding-agent"),
        "{listed}"
    );
    let err = w.fails(&["run", "say hello", "--harness", "pi", "-n", "old"]);
    assert!(
        err.contains("by: pi 0.73.1 is older than 0.80.4")
            && err.contains("Nothing was started.")
            && err.contains("next: npm install -g @earendil-works/pi-coding-agent"),
        "{err}"
    );
    assert!(w.branches().is_empty());
}

#[test]
fn a_key_branchyard_stored_counts_as_a_login_by_name_only() {
    let w = World::new();
    w.script("dsh", "#!/bin/sh\necho '0.2.1-rc.2'\n");
    let before = w.json(&["harnesses", "--json"]);
    assert_eq!(
        harness(&before, "deepseek-harness").unwrap()["login"]["state"],
        "logged_out"
    );
    let key = "dsh-test-0123456789";
    let mut child = w
        .command(BY)
        .args(["harnesses", "login", "deepseek-harness", "--api-key"])
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
    let after = w.by(&["harnesses", "--json", "--refresh"]);
    assert!(after.status.success(), "{}", text(&after.stderr));
    assert!(!text(&after.stdout).contains(key));
    let after: Value = serde_json::from_slice(&after.stdout).unwrap();
    let login = &harness(&after, "deepseek-harness").unwrap()["login"];
    assert_eq!(login["state"], "logged_in");
    assert_eq!(login["evidence"], "likely");
    assert_eq!(login["key"], "DEEPSEEK_API_KEY");
    assert_eq!(login["stored"], true);
    let table = w.ok(&["harnesses"]);
    let table = text(&table.stdout);
    assert!(table.contains("key stored (DEEPSEEK_API_KEY)"), "{table}");
    assert!(!table.contains(key));
}

/// A fake Claude Code: a version, and `claude auth status` saying logged
/// out. (2.1.310 itself answers `loggedIn: true` with a gateway token set,
/// and is then `logged in`, verified; the variable decides where the status
/// command says otherwise or nothing.)
const FAKE_CLAUDE: &str =
    "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '2.1.310 (Claude Code)'; \
     exit 0; fi\necho '{\"loggedIn\": false, \"authMethod\": \"none\"}'\n";

/// Claude Code behind a gateway reads `ANTHROPIC_AUTH_TOKEN`: the
/// inventory names it as the key a login rests on, and a key variable wins
/// over a status command that says logged out.
#[test]
fn a_gateway_token_in_the_environment_is_claude_codes_key() {
    let w = World::new();
    w.script("claude", FAKE_CLAUDE);
    let token = [("ANTHROPIC_AUTH_TOKEN", "zai-token-never-shown")];
    let out = w.by_env(&["harnesses", "--json"], &token);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!text(&out.stdout).contains("never-shown"));
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let login = &harness(&inventory, "claude-code").unwrap()["login"];
    assert_eq!(login["state"], "logged_in", "{login}");
    assert_eq!(login["key"], "ANTHROPIC_AUTH_TOKEN", "{login}");
    let table = text(&w.by_env(&["harnesses", "--refresh"], &token).stdout);
    assert!(table.contains("key set (ANTHROPIC_AUTH_TOKEN)"), "{table}");
}

/// `by harnesses login ID --provider zai`: one key for every harness on the
/// provider, the choice in `[logins]`, the export line for your own home,
/// and the listing counting it; never the key's value.
#[test]
fn a_provider_login_stores_one_key_and_records_each_harness_on_it() {
    let w = World::new();
    w.script("claude", FAKE_CLAUDE);
    w.script("qwen", "#!/bin/sh\necho '0.26.0'\n");
    let key = "zai-test-0123456789abcdef";
    let before = w.json(&["harnesses", "--json"]);
    assert_eq!(
        harness(&before, "claude-code").unwrap()["login"]["state"],
        "logged_out"
    );

    let out = w.by_input(
        &[
            "harnesses",
            "login",
            "claude-code",
            "--provider",
            "zai",
            "--json",
        ],
        key,
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!text(&out.stdout).contains(key) && !text(&out.stderr).contains(key));
    let said: Value = serde_json::from_slice(&out.stdout).unwrap();
    let file = w.user.parent().unwrap().join("secrets/ZAI_API_KEY");
    assert_eq!(said["status"], "stored", "{said}");
    assert_eq!(said["variable"], "ZAI_API_KEY");
    assert_eq!(said["key"], "stored");
    assert_eq!(said["secrets_entry_added"], true);
    assert_eq!(said["logins_entry"], "added");
    assert_eq!(
        said["isolated"],
        serde_json::json!({
            "env": ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL", "ENABLE_TOOL_SEARCH"],
            "model": "glm-4.6",
        })
    );
    assert_eq!(
        said["export"],
        format!(
            "export ANTHROPIC_AUTH_TOKEN=\"$(cat {})\" \
             ANTHROPIC_BASE_URL=https://api.z.ai/api/anthropic ENABLE_TOOL_SEARCH=false \
             ANTHROPIC_MODEL=glm-4.6",
            file.display()
        )
    );
    assert_eq!(said["config"], Value::Null);
    assert_eq!(fs::read_to_string(&file).unwrap(), key);
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let config = fs::read_to_string(&w.user).unwrap();
    assert!(
        config.contains(&format!("ZAI_API_KEY = \"@{}\"", file.display())),
        "{config}"
    );
    assert!(
        config.contains("[logins]\nclaude-code = \"zai\""),
        "{config}"
    );
    assert!(!config.contains(key));
    w.ok(&["config", "validate"]);

    // A second harness on the same provider: the stored key, nothing read.
    let out = w.ok(&["harnesses", "login", "qwen-code", "--provider", "zai"]);
    let said = text(&out.stdout);
    assert!(
        said.contains(&format!(
            "qwen-code uses the ZAI_API_KEY stored in {}",
            file.display()
        )),
        "{said}"
    );
    assert!(
        said.contains(
            "qwen-code through Z.ai (GLM): a branch with a private home (--isolated, a \
             sandbox) gets OPENAI_API_KEY (the key), \
             OPENAI_BASE_URL=https://api.z.ai/api/coding/paas/v4, and model glm-4.5-air"
        ),
        "{said}"
    );
    assert!(
        said.contains(
            "OPENAI_BASE_URL=https://api.z.ai/api/coding/paas/v4 OPENAI_MODEL=glm-4.5-air"
        ),
        "{said}"
    );
    assert!(!said.contains(key));
    let config = fs::read_to_string(&w.user).unwrap();
    assert!(
        config.contains("[logins]\nqwen-code = \"zai\"\nclaude-code = \"zai\""),
        "{config}"
    );
    // Again: nothing changes.
    let again = w.json(&[
        "harnesses",
        "login",
        "qwen-code",
        "--provider",
        "zai",
        "--json",
    ]);
    assert_eq!(again["key"], "reused");
    assert_eq!(again["logins_entry"], "unchanged");
    assert_eq!(fs::read_to_string(&w.user).unwrap(), config);

    // The listing counts the login, by the key's name only.
    let out = w.ok(&["harnesses", "--json", "--refresh"]);
    assert!(!text(&out.stdout).contains(key));
    let after: Value = serde_json::from_slice(&out.stdout).unwrap();
    let login = &harness(&after, "claude-code").unwrap()["login"];
    assert_eq!(login["state"], "logged_in", "{login}");
    assert_eq!(login["key"], "ZAI_API_KEY");
    assert_eq!(login["stored"], true);
    let table = text(&w.ok(&["harnesses"]).stdout);
    assert!(
        table
            .lines()
            .any(|l| l.starts_with("claude-code") && l.contains("key stored (ZAI_API_KEY)")),
        "{table}"
    );
    let log: Value = w.json(&["harnesses", "log", "--json"]);
    let outcomes: Vec<&str> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["outcome"].as_str().unwrap())
        .collect();
    assert!(outcomes.contains(&"provider"), "{log}");
    assert!(!log.to_string().contains(key));

    // What there is not, said with what there is.
    let why = w.fails(&["harnesses", "login", "claude-code", "--provider", "openai"]);
    assert!(
        why.contains("no provider named \"openai\"; providers are zai, gemini"),
        "{why}"
    );
    let why = w.fails(&["harnesses", "login", "claude-code", "--provider", "gemini"]);
    assert!(
        why.contains("claude-code has no gemini login; it has zai"),
        "{why}"
    );
    let why = w.fails(&[
        "harnesses",
        "login",
        "claude-code",
        "--provider",
        "zai",
        "--on",
        "ssh://vm.test",
    ]);
    assert!(
        why.contains("a provider login is kept on the machine that runs the harness"),
        "{why}"
    );
}

/// Codex takes no endpoint from its environment: the login offers its
/// `config.toml` change for your own home, shows it, and writes it only
/// with `--yes` (or a yes on a terminal).
#[test]
fn a_codex_provider_login_shows_its_config_change_and_writes_it_with_yes() {
    let w = World::new();
    w.fake_codex(&w.bin, "0.164.0");
    let config = w.home.join(".codex/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let original = "approval_policy = \"untrusted\"\n";
    fs::write(&config, original).unwrap();
    let key = "zai-test-0123456789abcdef";

    // No terminal, no --yes: stored, the change shown, the file unchanged.
    let out = w.by_input(&["harnesses", "login", "codex", "--provider", "zai"], key);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let said = text(&out.stdout);
    assert!(said.contains("+model_provider = \"zai\""), "{said}");
    assert!(said.contains("+[model_providers.zai]"), "{said}");
    assert!(
        said.contains("Not written: there is no terminal to ask on. Add --yes"),
        "{said}"
    );
    assert!(said.contains("export ZAI_API_KEY=\"$(cat "), "{said}");
    assert!(!said.contains(key));
    assert_eq!(fs::read_to_string(&config).unwrap(), original);
    let shown = w.json(&["harnesses", "login", "codex", "--provider", "zai", "--json"]);
    assert_eq!(shown["config"]["wrote"], false, "{shown}");
    assert_eq!(shown["config"]["changed"], true);
    assert_eq!(shown["config"]["file"], "~/.codex/config.toml");

    // --yes: written, the rest kept.
    let out = w.ok(&["harnesses", "login", "codex", "--provider", "zai", "--yes"]);
    assert!(
        text(&out.stdout).contains("wrote ~/.codex/config.toml"),
        "{}",
        text(&out.stdout)
    );
    let written = fs::read_to_string(&config).unwrap();
    for line in [
        "approval_policy = \"untrusted\"",
        "model = \"glm-4.6\"",
        "model_provider = \"zai\"",
        "[model_providers.zai]",
        "base_url = \"https://api.z.ai/api/v1\"",
        "env_key = \"ZAI_API_KEY\"",
        "wire_api = \"responses\"",
    ] {
        assert!(written.contains(line), "{line}: {written}");
    }
    let out = w.ok(&["harnesses", "login", "codex", "--provider", "zai", "--yes"]);
    assert!(
        text(&out.stdout).contains("~/.codex/config.toml selects zai already"),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(fs::read_to_string(&config).unwrap(), written);
}

/// An isolated run of a harness logged in to Z.ai gets the login's
/// variables and the stored key, and nothing of the key under its own
/// name: the fake ACP agent standing in for Qwen Code prints what it got.
#[test]
fn an_isolated_run_gets_the_provider_login_in_its_environment() {
    let w = World::new();
    let agent = fake_agent!().display().to_string();
    w.script(
        "qwen",
        &format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 0.26.0; exit 0; fi\nexec {agent}\n"
        ),
    );
    let key = "zai-test-0123456789abcdef";
    let out = w.by_input(
        &["harnesses", "login", "qwen-code", "--provider", "zai"],
        key,
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = w.ok(&[
        "run",
        "ENV OPENAI_API_KEY OPENAI_BASE_URL OPENAI_MODEL ZAI_API_KEY",
        "--harness",
        "qwen-code",
        "--isolated",
        "-n",
        "glm",
    ]);
    let all = format!("{}{}", text(&out.stdout), text(&out.stderr));
    let log = text(&w.ok(&["log", "glm"]).stdout);
    for line in [
        format!("OPENAI_API_KEY={key}"),
        "OPENAI_BASE_URL=https://api.z.ai/api/coding/paas/v4".to_owned(),
        "OPENAI_MODEL=glm-4.5-air".to_owned(),
        "ZAI_API_KEY unset".to_owned(),
    ] {
        assert!(log.contains(&line), "{line}\n{log}\n{all}");
    }
    // Without --isolated nothing of the login reaches the harness.
    let out = w.ok(&[
        "run",
        "ENV OPENAI_API_KEY OPENAI_BASE_URL",
        "--harness",
        "qwen-code",
        "-n",
        "mine",
    ]);
    let log = text(&w.ok(&["log", "mine"]).stdout);
    assert!(
        log.contains("OPENAI_API_KEY unset") && log.contains("OPENAI_BASE_URL unset"),
        "{log}\n{}",
        text(&out.stderr)
    );
}

/// Antigravity's `agy` reads `GEMINI_API_KEY` only with `modelProvider` set
/// to `gemini` in its settings; `by harnesses login antigravity` writes
/// that one key, after showing the change.
#[test]
fn login_antigravity_writes_the_model_provider_when_a_gemini_key_is_set() {
    let w = World::new();
    w.script("agy", "#!/bin/sh\necho '1.4.7'\n");
    let settings = w.home.join(".gemini/antigravity-cli/settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let original =
        "{\n    \"theme\": \"dark\",\n    \"toolPermissions\": {\"allow\": [\"read\"]}\n}\n";
    fs::write(&settings, original).unwrap();
    let key = [("GEMINI_API_KEY", "gem-test-0123456789")];

    // The listing says the key is set and agy will not use it yet.
    let out = w.by_env(&["harnesses"], &key);
    let table = text(&out.stdout);
    assert!(table.contains("key set, unused"), "{table}");
    assert!(!table.contains("key set (GEMINI_API_KEY)"), "{table}");
    assert!(
        table.contains("`by harnesses login antigravity` writes modelProvider gemini"),
        "{table}"
    );

    // `by doctor` and `by setup` name the step; `by run` is refused before
    // a branch exists (agy would ask for a Google sign-in and fail), and
    // goes on once the setting is there or with --no-preflight.
    let doctor = text(&w.by_env(&["doctor"], &key).stdout);
    assert!(
        doctor
            .lines()
            .any(|l| l.starts_with("todo") && l.contains("harnesses.antigravity")),
        "{doctor}"
    );
    assert!(
        doctor.contains("run `by harnesses login antigravity`"),
        "{doctor}"
    );
    let setup = w.by_env(&["setup", "--dry-run"], &key);
    let setup = text(&setup.stdout);
    assert!(
        setup.contains("reads it only with modelProvider gemini")
            && setup.contains("shows the change and writes it"),
        "{setup}"
    );
    let out = w.by_env(
        &[
            "run",
            "say hi",
            "--harness",
            "antigravity",
            "--allow-unapproved-tools",
            "--yes",
            "-n",
            "agy1",
        ],
        &key,
    );
    assert!(!out.status.success());
    let why = text(&out.stderr);
    assert!(why.contains("does not set modelProvider gemini"), "{why}");
    assert!(
        why.contains("next: by harnesses login antigravity"),
        "{why}"
    );
    assert!(why.contains("Nothing was started"), "{why}");
    assert!(w.branches().is_empty());

    // No terminal and no --yes: the change is shown, nothing is written.
    let out = w.by_env(&["harnesses", "login", "antigravity"], &key);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    let shown = text(&out.stdout);
    assert!(
        shown.contains("+    \"modelProvider\": \"gemini\""),
        "{shown}"
    );
    assert!(
        shown.contains("harnesses login antigravity --yes"),
        "{shown}"
    );
    assert!(!shown.contains("gem-test-0123456789"));
    assert_eq!(fs::read_to_string(&settings).unwrap(), original);

    // --yes: merged, everything else kept, indentation kept.
    let out = w.by_env(&["harnesses", "login", "antigravity", "--yes"], &key);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("wrote ~/.gemini/antigravity-cli/settings.json"));
    let written = fs::read_to_string(&settings).unwrap();
    let value: Value = serde_json::from_str(&written).unwrap();
    assert_eq!(value["modelProvider"], "gemini");
    assert_eq!(value["theme"], "dark");
    assert_eq!(value["toolPermissions"]["allow"][0], "read");
    assert!(written.contains("\n    \"theme\""), "{written}");

    // Detection now agrees; a second login has nothing to do.
    let table = text(&w.by_env(&["harnesses", "--refresh"], &key).stdout);
    assert!(table.contains("key set (GEMINI_API_KEY)"), "{table}");
    let inventory: Value =
        serde_json::from_slice(&w.by_env(&["harnesses", "--json", "--refresh"], &key).stdout)
            .unwrap();
    let login = &harness(&inventory, "antigravity").unwrap()["login"];
    assert_eq!(login["key"], "GEMINI_API_KEY");
    assert!(login["detail"]
        .as_str()
        .unwrap()
        .contains("selects the gemini provider"));
    let out = w.by_env(&["harnesses", "login", "antigravity", "--yes"], &key);
    assert!(out.status.success());
    assert!(text(&out.stdout).contains("already reads GEMINI_API_KEY"));
    assert_eq!(fs::read_to_string(&settings).unwrap(), written);

    // A provider already chosen is never replaced.
    fs::write(
        &settings,
        "{\"modelProvider\": \"vertex\", \"theme\": \"dark\"}\n",
    )
    .unwrap();
    let out = w.by_env(&["harnesses", "login", "antigravity", "--yes"], &key);
    assert!(!out.status.success());
    let why = text(&out.stderr);
    assert!(
        why.contains("modelProvider") && why.contains("different value"),
        "{why}"
    );
    assert!(fs::read_to_string(&settings).unwrap().contains("vertex"));

    // No settings file at all: created, with just that key.
    fs::remove_file(&settings).unwrap();
    let out = w.by_env(
        &["harnesses", "login", "antigravity", "--yes", "--json"],
        &key,
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["wrote"], true);
    assert_eq!(
        serde_json::from_str::<Value>(&fs::read_to_string(&settings).unwrap()).unwrap(),
        serde_json::json!({"modelProvider": "gemini"})
    );
}

#[test]
fn login_prints_the_exact_command_for_a_harness_it_cannot_log_in_itself() {
    let w = World::new();
    w.script("qwen", "#!/bin/sh\necho '0.26.0'\n");
    w.script("pi", "#!/bin/sh\necho 'pi 0.87.1'\n");
    w.script("claude", "#!/bin/sh\necho '2.1.283'\n");
    // No login command, no key variable: what to do, not a dead end.
    let why = w.fails(&["harnesses", "login", "qwen-code"]);
    assert!(
        why.contains("start `qwen` and sign in inside it, then `by harnesses --refresh`"),
        "{why}"
    );
    // ... or another provider's key, where the harness takes one.
    assert!(
        why.contains("`by harnesses login qwen-code --provider zai`"),
        "{why}"
    );
    // No login command but key variables (Pi reads ZAI_API_KEY and the
    // rest): the key, or signing in inside it.
    let why = w.fails(&["harnesses", "login", "pi"]);
    assert!(
        why.contains(
            "give it an API key with `by harnesses login pi --api-key` (ANTHROPIC_API_KEY"
        ) && why.contains("or sign in inside `pi` itself"),
        "{why}"
    );
    // A catalog login command: the exact command, without a terminal.
    let out = w.by(&["harnesses", "login", "claude-code"]);
    let said = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(
        said.contains("run `claude auth login` in a terminal"),
        "{said}"
    );
}

#[test]
fn login_runs_the_harness_own_flow_or_stores_a_key_and_says_what_to_run() {
    let w = World::new();
    w.fake_codex(&w.bin, "0.157.1");
    // No terminal: nothing ran, so it says what to run and exits 3, not 0,
    // and `by harnesses login codex && by run ...` stops here.
    let out = w.by(&["harnesses", "login", "codex"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("no terminal here; run `codex login --device-auth`"),
        "{}",
        text(&out.stderr)
    );
    assert!(text(&out.stdout).is_empty(), "{}", text(&out.stdout));
    assert!(w.calls("codex login --device-auth").is_empty());
    // With --json: the same exit, and the status on stdout.
    let out = w.by(&["harnesses", "login", "codex", "--json"]);
    assert_eq!(out.status.code(), Some(3));
    let said: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(said["status"], "manual", "{said}");
    assert!(said["run"]
        .as_str()
        .unwrap()
        .contains("codex login --device-auth"));
    // A terminal: the harness's own flow, passed through.
    let out = in_pty(&w, &["harnesses", "login", "codex"]);
    assert!(out.contains("https://auth.example/device"), "{out}");
    assert!(
        out.contains("codex on this machine: logged in (verified"),
        "{out}"
    );
    assert_eq!(w.calls("codex login --device-auth").len(), 1);
    // Done, as --json says it.
    let out = in_pty(&w, &["harnesses", "login", "codex", "--json"]);
    let start = out.find('{').unwrap();
    let done: Value = serde_json::from_str(&out[start..]).unwrap();
    assert_eq!(done["status"], "done", "{out}");
    assert_eq!(done["login"]["state"], "logged_in", "{out}");
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
    // Stored, as --json says it (the same key again: nothing new is named).
    let mut again = w
        .command(BY)
        .args(["harnesses", "login", "claude-code", "--api-key", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    again
        .stdin
        .take()
        .unwrap()
        .write_all(key.as_bytes())
        .unwrap();
    let again = again.wait_with_output().unwrap();
    assert!(again.status.success(), "{}", text(&again.stderr));
    let stored: Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(stored["status"], "stored", "{stored}");
    assert_eq!(stored["secrets_entry_added"], false);
    assert!(!text(&again.stdout).contains(key));
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
    assert_eq!(
        outcomes,
        ["manual", "manual", "ran", "ran", "stored_key", "stored_key"]
    );
    assert!(!serde_json::to_string(&log).unwrap().contains(key));
    // No login command in the catalog but a key variable: nothing ran either,
    // and it says which form works.
    let out = w.by(&["harnesses", "login", "deepseek-harness"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr)
            .contains("give it an API key with `by harnesses login deepseek-harness --api-key`"),
        "{}",
        text(&out.stderr)
    );
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

/// Before `by run`, `by fan` and `by send` create a branch: a harness
/// that is not installed, or is verified logged out, is refused with the
/// command to run; a guess, a named executable and `--no-preflight` go on.
#[test]
fn a_run_is_refused_before_a_branch_when_its_harness_cannot_start() {
    let w = World::new();
    // Installed, and its own status command says it is logged out.
    w.fake_codex(&w.bin, "0.157.1");
    let why = w.fails(&["run", "say hi", "--harness", "codex", "-n", "cx"]);
    assert!(
        why.contains(
            "by: codex is verified logged out (codex login status: Not logged in). Nothing was \
             started."
        ),
        "{why}"
    );
    assert!(
        why.contains("next: run `by harnesses login codex`"),
        "{why}"
    );
    assert!(why.contains("--no-preflight"), "{why}");
    // Refused before the yard is opened: no `.branchyard/` either.
    assert!(!w.repo.join(".branchyard").exists());
    assert!(w.branches().is_empty());
    assert!(w.calls("app-server").is_empty());
    // With --json the refusal is the error envelope on stdout, with its next.
    let out = w.by(&["run", "say hi", "--harness", "codex", "-n", "cx", "--json"]);
    // The pre-flight prints its own refusal; the process still exits with
    // the status the envelope names (environment: 3).
    assert_eq!(out.status.code(), Some(3));
    let error: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(error["error"]["kind"], "harness_unavailable", "{error}");
    assert_eq!(error["error"]["exit"], 3, "{error}");
    assert!(
        error["error"]["next"][0]
            .as_str()
            .unwrap_or_default()
            .contains("by harnesses login codex"),
        "{error}"
    );
    // A key named by --secret reaches the harness only in a private home:
    // the refusal says to add --isolated.
    let why = w.fails(&[
        "run",
        "say hi",
        "--harness",
        "codex",
        "-n",
        "cx",
        "--secret",
        "OPENAI_API_KEY=MY_KEY",
    ]);
    assert!(
        why.contains("add --isolated, so the run gets OPENAI_API_KEY"),
        "{why}"
    );
    // A fan refuses the same, whichever harness is among the list.
    let why = w.fails(&["fan", "say hi", "--harness", "gemini-cli,codex"]);
    assert!(why.contains("gemini-cli is not installed"), "{why}");
    assert!(w.branches().is_empty());
    let why = w.fails(&["fan", "say hi", "--harness", "codex"]);
    assert!(why.contains("codex is verified logged out"), "{why}");
    assert!(w.branches().is_empty());
    // --no-preflight tries anyway: Codex is started (the fake cannot speak
    // its protocol, so the branch fails, after it exists).
    w.by(&[
        "run",
        "say hi",
        "--harness",
        "codex",
        "-n",
        "cx",
        "--no-preflight",
        "--yes",
    ]);
    assert_eq!(w.branches(), ["cx"]);
    assert!(!w.calls("app-server").is_empty());
    // Logged in: nothing is said, and the harness starts.
    fs::write(w.bin.join("codex.status"), "Logged in using ChatGPT\n").unwrap();
    let out = w.by(&["run", "again", "--harness", "codex", "-n", "cx2", "--yes"]);
    assert!(
        !text(&out.stderr).contains("Nothing was started"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(w.branches(), ["cx", "cx2"]);
}

/// A key the isolated run will get (`--secret`, `[secrets]`, or one `by
/// harnesses login --api-key` stored) is a login the host's status command
/// cannot see: the run goes on with a note instead of being refused.
#[test]
fn a_provisioned_key_lets_an_isolated_run_past_a_verified_logout() {
    let w = World::new();
    w.fake_codex(&w.bin, "0.157.1");
    // The fake cannot speak Codex's protocol, so the branch fails after it
    // exists; that it exists is the point.
    let out = w.by_env(
        &[
            "run",
            "say hi",
            "--harness",
            "codex",
            "-n",
            "cx",
            "--isolated",
            "--secret",
            "OPENAI_API_KEY=MY_KEY",
            "--yes",
        ],
        &[("MY_KEY", "sk-test-0123456789")],
    );
    let err = text(&out.stderr);
    assert!(
        err.contains("by: note: codex is logged out here; the isolated run gets OPENAI_API_KEY"),
        "{err}"
    );
    assert!(!err.contains("Nothing was started"), "{err}");
    assert_eq!(w.branches(), ["cx"]);
    // A key Branchyard stored counts too, whether or not the listing's cache
    // is fresh.
    let mut child = w
        .command(BY)
        .args(["harnesses", "login", "codex", "--api-key"])
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
        .write_all(b"sk-test-9876543210")
        .unwrap();
    let stored = child.wait_with_output().unwrap();
    assert!(stored.status.success(), "{}", text(&stored.stderr));
    let out = w.by(&[
        "run",
        "say hi",
        "--harness",
        "codex",
        "-n",
        "cx2",
        "--isolated",
        "--yes",
    ]);
    assert!(
        !text(&out.stderr).contains("Nothing was started"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(w.branches(), ["cx", "cx2"]);
}

#[test]
fn a_harness_that_is_not_installed_is_refused_with_how_to_install_it() {
    let w = World::new();
    // No terminal and no setting: installs are off, so the catalog's own
    // command, and the file and key that would turn them on.
    let why = w.fails(&["run", "x", "--harness", "qwen-code"]);
    assert!(
        why.contains("qwen-code is not installed on this machine."),
        "{why}"
    );
    assert!(
        why.contains("next: npm install -g @qwen-code/qwen-code"),
        "{why}"
    );
    assert!(
        why.contains(&format!(
            "set install = \"ask\" under [harnesses] in {}",
            w.user.display()
        )),
        "{why}"
    );
    // Allowed to ask, with no terminal to ask on: --yes.
    w.user_config("[harnesses]\ninstall = \"ask\"\n");
    let why = w.fails(&["run", "x", "--harness", "qwen-code"]);
    assert!(
        why.contains("next: by harnesses install qwen-code --yes"),
        "{why}"
    );
    // Allowed without asking: the command itself.
    w.user_config("[harnesses]\ninstall = \"auto\"\n");
    let why = w.fails(&["run", "x", "--harness", "qwen-code"]);
    assert!(
        why.contains("next: by harnesses install qwen-code\n"),
        "{why}"
    );
    // Without a harness named, the built-in default is the one refused, and
    // the way to name another is said.
    let why = w.fails(&["run", "x"]);
    assert!(why.contains("claude-code is not installed"), "{why}");
    assert!(
        why.contains("`by run` uses claude-code unless --harness"),
        "{why}"
    );
    // A branch keeps its harness: a send checks that one (here a gemini that
    // has gone since the branch was made), and --json says so as the error
    // it is.
    w.fake_gemini();
    w.ok(&[
        "run",
        "WRITE a.txt=1",
        "-n",
        "keep",
        "--harness",
        "gemini-cli",
        "--yes",
    ]);
    fs::remove_file(w.bin.join("gemini")).unwrap();
    let why = w.fails(&["send", "keep", "more"]);
    assert!(why.contains("gemini-cli is not installed"), "{why}");
    let out = w.by(&["send", "keep", "more", "--json"]);
    assert_eq!(out.status.code(), Some(3));
    let error: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(error["error"]["kind"], "harness_unavailable", "{error}");
    // The executable named on the command line is the person's business.
    let agent = fake_agent!().display().to_string();
    w.ok(&["send", "keep", "more", "--command", &agent, "--yes"]);
    assert_eq!(w.branches(), ["keep"]);
}

#[test]
fn a_guess_that_a_harness_is_logged_out_is_a_note_not_a_refusal() {
    let w = World::new();
    // Installed, no login file and no key: "likely" logged out.
    w.fake_gemini();
    let out = w.ok(&[
        "run",
        "WRITE g.txt=1",
        "-n",
        "g",
        "--harness",
        "gemini-cli",
        "--yes",
    ]);
    let err = text(&out.stderr);
    assert!(
        err.contains("by: note: gemini-cli is likely logged out"),
        "{err}"
    );
    assert!(err.contains("Going on, since that is a guess"), "{err}");
    let show = w.json(&["show", "g", "--json"]);
    assert_eq!(show["status"]["state"], "ready", "{show}");
    // A key variable is a login: nothing is said.
    let out = w.by_env(
        &[
            "run",
            "WRITE h.txt=1",
            "-n",
            "h",
            "--harness",
            "gemini-cli",
            "--yes",
        ],
        &[("GEMINI_API_KEY", "not-a-key")],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        !text(&out.stderr).contains("note:"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn a_named_executable_or_a_harness_nothing_drives_skips_or_explains_the_check() {
    let w = World::new();
    let agent = fake_agent!().display().to_string();
    // Not installed, but --command names the executable: no check.
    w.ok(&[
        "run",
        "WRITE a.txt=1",
        "-n",
        "named",
        "--harness",
        "qwen-code",
        "--command",
        &agent,
        "--yes",
    ]);
    // A branch started that way keeps the executable for its later turns.
    w.ok(&["send", "named", "more", "--yes"]);
    // Known to Branchyard, not drivable: not "no harness named".
    let why = w.fails(&["run", "x", "--harness", "droid"]);
    assert!(
        why.contains("droid is known to Branchyard but not drivable"),
        "{why}"
    );
    assert!(why.contains("drivable: claude-code, codex, "), "{why}");
    assert!(!why.contains("no harness or profile named droid"), "{why}");
    // Not known at all: the name, and what would have worked.
    let why = w.fails(&["run", "x", "--harness", "nope"]);
    assert!(
        why.contains("no harness or profile named nope; drivable: claude-code, "),
        "{why}"
    );
    assert_eq!(w.branches(), ["named"]);
}

/// `by harnesses --profiles` is a static table: it needs no repository.
#[test]
fn the_profile_table_works_outside_a_repository() {
    let w = World::new();
    let outside = w.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let run = |args: &[&str]| {
        let out = w
            .command(BY)
            .current_dir(&outside)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        text(&out.stdout)
    };
    let table = run(&["harnesses", "--profiles"]);
    assert!(table.contains("claude-code-stream-json"), "{table}");
    assert!(table.contains("cursor-acp"), "{table}");
    let profiles: Value =
        serde_json::from_str(&run(&["harnesses", "--profiles", "--json"])).unwrap();
    assert!(profiles
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["profile"] == "codex-app-server"));
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
    let out = remote(&["harnesses", "login", "codex"]);
    let said = text(&out.stdout);
    assert!(
        said.contains("run `codex login --device-auth` there, in a terminal"),
        "{said}"
    );
    // Nothing was logged in: exit 3, as without a terminal here.
    assert_eq!(out.status.code(), Some(3), "{said}");
    let out = remote(&["harnesses", "login", "codex", "--json"]);
    assert_eq!(out.status.code(), Some(3));
    let said: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(said["status"], "manual", "{said}");
    // Workers never install.
    let out = remote(&["harnesses", "install", "codex"]);
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("never install harnesses themselves"));
    assert!(!remote(&["harnesses", "--on", "ssh://vm.test"])
        .status
        .success());
    branchyard_support::best_effort(
        "stop the server process",
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status(),
    );
    branchyard_support::best_effort("reap the server process", child.wait());
}

/// A Codex with a `model_providers` entry of its own and no OpenAI login:
/// its status command says "Not logged in", and the run is fine on the
/// provider's key. Detection reads the selected provider's `env_key` out of
/// `config.toml` (by name), so the listing and the pre-flight agree with it.
#[test]
fn a_codex_model_providers_key_is_a_login_the_preflight_accepts() {
    let w = World::new();
    w.fake_codex(&w.bin, "0.164.0");
    fs::create_dir_all(w.home.join(".codex")).unwrap();
    fs::write(
        w.home.join(".codex/config.toml"),
        "model = \"glm-4.6\"\nmodel_provider = \"zai\"\n\n[model_providers.other]\nenv_key = \"OTHER_KEY\"\n\n\
         [model_providers.zai]\nname = \"Z.ai\"\nbase_url = \"https://example.invalid/v1\"\n\
         wire_api = \"responses\"\nenv_key = \"ZAI_KEY\"\n\n[profiles.p]\nmodel_provider = \"nope\"\n",
    )
    .unwrap();
    // The provider is selected, its key is not set: logged out, and the
    // listing says what the configuration lacks.
    let inventory = w.json(&["harnesses", "--json", "--refresh"]);
    let codex = harness(&inventory, "codex").unwrap();
    assert_eq!(codex["login"]["state"], "logged_out", "{codex}");
    let detail = codex["login"]["detail"].as_str().unwrap();
    assert!(
        detail.contains(
            "selects the model provider zai, which reads ZAI_KEY, and ZAI_KEY is not set"
        ),
        "{detail}"
    );
    let why = w.fails(&["run", "say hi", "--harness", "codex", "-n", "cx"]);
    assert!(why.contains("ZAI_KEY is not set"), "{why}");
    assert!(why.contains("--no-preflight"), "{why}");
    assert!(w.branches().is_empty());
    // With the provider's key set, Codex is logged in by it, and the
    // pre-flight lets the run start (the fake cannot speak the protocol, so
    // the branch fails after it exists).
    let out = w.by_env(
        &["harnesses", "--json", "--refresh"],
        &[("ZAI_KEY", "sk-test-0123456789")],
    );
    let inventory: Value = serde_json::from_slice(&out.stdout).unwrap();
    let codex = harness(&inventory, "codex").unwrap();
    assert_eq!(codex["login"]["state"], "logged_in", "{codex}");
    assert_eq!(codex["login"]["key"], "ZAI_KEY");
    assert!(!text(&out.stdout).contains("sk-test-0123456789"));
    let out = w.by_env(
        &["run", "say hi", "--harness", "codex", "-n", "cx", "--yes"],
        &[("ZAI_KEY", "sk-test-0123456789")],
    );
    assert!(
        !text(&out.stderr).contains("Nothing was started"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(w.branches(), ["cx"]);
}
