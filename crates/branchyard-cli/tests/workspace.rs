//! `[workspace]` through the built `by`, against temporary repositories and
//! the fake ACP agent: the trust decision (refused without a terminal,
//! granted by `by workspace trust` or on a terminal, asked again after a
//! change, never needed for copy-only or your own user file, never taken
//! by a harness), `by workspace show` and `run`, and teardown on `by rm`
//! and `by merge --rm`. Hermetic; requires `git` and `sh`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use serde_json::{json, Value};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn fake_agent() -> &'static Path {
    static AGENT: OnceLock<PathBuf> = OnceLock::new();
    AGENT.get_or_init(|| {
        let by = PathBuf::from(env!("CARGO_BIN_EXE_by"));
        let profile_dir = by.parent().unwrap().to_path_buf();
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut command = Command::new(cargo);
        command
            .args(["build", "--quiet", "--offline", "--manifest-path"])
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"))
            .args(["-p", "branchyard-runtime", "--bin", "fake-acp-agent"])
            .env("CARGO_TARGET_DIR", profile_dir.parent().unwrap());
        match profile_dir.file_name().and_then(|n| n.to_str()) {
            Some("debug") => {}
            Some("release") => {
                command.arg("--release");
            }
            Some(other) => {
                command.args(["--profile", other]);
            }
            None => panic!("unexpected binary location {}", by.display()),
        }
        assert!(command.status().unwrap().success());
        profile_dir.join("fake-acp-agent")
    })
}

struct Repo {
    dir: PathBuf,
    root: PathBuf,
}

const WORKSPACE: &str = r#"
[workspace]
copy = [".env"]
setup = "echo $BRANCHYARD_PORT > setup-port.txt"
teardown = "echo $BRANCHYARD_BRANCH >> $BRANCHYARD_ROOT/../teardown.log"

[workspace.run.dev]
command = "echo serving on $BRANCHYARD_PORT in $BRANCHYARD_WORKTREE"
default = true

[workspace.run.fail]
command = "exit 4"
"#;

impl Repo {
    fn new(project: &str) -> Repo {
        let dir = std::env::temp_dir().join(format!(
            "branchyard-cli-workspace-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let repo = Repo {
            root: dir.join("repo"),
            dir,
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "user.email", "test@localhost"]);
        fs::write(repo.root.join("a.txt"), "one\n").unwrap();
        fs::write(repo.root.join(".gitignore"), ".env\nbranchyard.toml\n").unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "-q", "-m", "initial"]);
        fs::write(repo.root.join(".env"), "TOKEN_NAME=x\n").unwrap();
        fs::write(repo.root.join("branchyard.toml"), project).unwrap();
        repo
    }

    fn user_config(&self) -> PathBuf {
        self.dir.join("user/config.toml")
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env("BRANCHYARD_USER_CONFIG", self.user_config())
            .env_remove("BRANCHYARD_TRUST_FILE")
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
        command
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self.command("git").args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn by(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_by"))
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn by_agent(&self, args: &[&str]) -> Output {
        let agent = fake_agent().display().to_string();
        let mut all: Vec<&str> = args.to_vec();
        all.extend(["--command", &agent, "--yes"]);
        if args[0] != "send" {
            all.extend(["--harness", "gemini-cli"]);
        }
        self.by(&all)
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.by(args);
        assert!(out.status.success(), "{}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn branches(&self) -> usize {
        self.json(&["ls", "--json"]).as_array().unwrap().len()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn untrusted_scripts_are_refused_without_a_terminal_until_trusted_and_again_after_a_change() {
    let repo = Repo::new(WORKSPACE);
    let out = repo.by_agent(&["run", "WRITE x.txt=1", "--name", "first"]);
    assert_eq!(out.status.code(), Some(1));
    let said = stderr(&out);
    assert!(said.contains("has scripts you have not trusted"), "{said}");
    assert!(said.contains("by workspace trust"), "{said}");
    assert!(said.contains("nothing was created"), "{said}");
    assert_eq!(repo.branches(), 0);
    let shown = repo.json(&["workspace", "show", "--json"]);
    assert_eq!(shown["trust"], "untrusted");
    assert_eq!(shown["origin"]["kind"], "project");
    assert_eq!(shown["workspace"]["copy"][0], ".env");

    let trusted = repo.by(&["workspace", "trust"]);
    assert!(trusted.status.success(), "{}", stderr(&trusted));
    assert!(stdout(&trusted).contains("setup     echo $BRANCHYARD_PORT"));
    let trust_file = repo.dir.join("user/trusted-workspaces.json");
    let recorded: Value = serde_json::from_str(&fs::read_to_string(&trust_file).unwrap()).unwrap();
    assert_eq!(
        recorded["repositories"][repo.root.display().to_string()]["digest"],
        shown["digest"]
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&trust_file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        repo.json(&["workspace", "show", "--json"])["trust"],
        "trusted"
    );

    let out = repo.by_agent(&["run", "WRITE x.txt=1", "--name", "first"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out).contains("workspace setup: ok"),
        "{}",
        stdout(&out)
    );
    let info = repo.json(&["workspace", "show", "first", "--json"]);
    assert_eq!(info["ready"], true);
    let port = info["port"].as_u64().unwrap();
    let worktree = PathBuf::from(info["worktree"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(worktree.join("setup-port.txt"))
            .unwrap()
            .trim(),
        port.to_string()
    );
    assert!(worktree.join(".env").is_file());

    // A changed script asks again, and is refused here.
    let changed = WORKSPACE.replace("setup-port.txt", "setup-port.txt; curl example.invalid");
    fs::write(repo.root.join("branchyard.toml"), changed).unwrap();
    let out = repo.by_agent(&["run", "WRITE y.txt=1", "--name", "second"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("changed since you trusted it"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        repo.json(&["workspace", "show", "--json"])["trust"],
        "changed"
    );
    // A send creates nothing, so it asks nothing.
    let sent = repo.by_agent(&["send", "first", "WHOAMI"]);
    assert!(sent.status.success(), "{}", stderr(&sent));

    // Untrusting forgets it.
    assert!(repo.by(&["workspace", "untrust"]).status.success());
    let recorded: Value = serde_json::from_str(&fs::read_to_string(&trust_file).unwrap()).unwrap();
    assert!(recorded["repositories"].as_object().unwrap().is_empty());
}

#[test]
fn copy_only_and_your_own_user_file_need_no_trust() {
    let repo = Repo::new("[workspace]\ncopy = [\".env\"]\n");
    let out = repo.by_agent(&["run", "WRITE x.txt=1", "--name", "copied"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let info = repo.json(&["workspace", "show", "copied", "--json"]);
    assert_eq!(info["copied"][0], ".env");
    assert!(
        info["port"].as_u64().is_some(),
        "a [workspace] reserves a port"
    );

    // The user file's entry replaces the repository's section, for you.
    fs::create_dir_all(repo.dir.join("user")).unwrap();
    fs::write(
        repo.user_config(),
        format!(
            "[projects.\"{}\".workspace]\nsetup = \"touch mine\"\n",
            repo.root.display()
        ),
    )
    .unwrap();
    fs::write(repo.root.join("branchyard.toml"), WORKSPACE).unwrap();
    let shown = repo.json(&["workspace", "show", "--json"]);
    assert_eq!(shown["origin"]["kind"], "user");
    assert_eq!(shown["trust"], "not_needed");
    let out = repo.by_agent(&["run", "WRITE y.txt=1", "--name", "mine"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let info = repo.json(&["workspace", "show", "mine", "--json"]);
    let worktree = PathBuf::from(info["worktree"].as_str().unwrap());
    assert!(worktree.join("mine").is_file());
    assert!(
        !worktree.join(".env").exists(),
        "the project's section was replaced"
    );

    // [workspace] belongs in the project file, [projects] in the user file.
    fs::write(repo.user_config(), "[workspace]\nsetup = \"x\"\n").unwrap();
    let out = repo.by(&["config", "validate"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("belongs in a repository's branchyard.toml"));
}

#[test]
fn a_harness_on_a_branch_can_never_trust_scripts() {
    let repo = Repo::new(WORKSPACE);
    let out = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .args(["workspace", "trust"])
        .env("BRANCHYARD_BRANCH", "some-branch")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("a person's decision"),
        "{}",
        stderr(&out)
    );
    assert!(!repo.dir.join("user/trusted-workspaces.json").exists());
}

#[test]
fn run_scripts_teardown_on_rm_and_merge_rm() {
    let repo = Repo::new(WORKSPACE);
    assert!(repo.by(&["workspace", "trust"]).status.success());
    let out = repo.by_agent(&["run", "WRITE x.txt=1", "--name", "served"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let info = repo.json(&["workspace", "show", "served", "--json"]);
    let port = info["port"].as_u64().unwrap();

    let out = repo.by(&["workspace", "run", "served"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim(),
        format!(
            "serving on {port} in {}",
            info["worktree"].as_str().unwrap()
        )
    );
    let out = repo.by(&["workspace", "run", "served", "fail"]);
    assert_eq!(out.status.code(), Some(1));
    let out = repo.by(&["workspace", "run", "served", "nope"]);
    assert!(stderr(&out).contains("no run script named nope"));
    let out = repo.by(&["workspace", "run"]);
    assert!(stderr(&out).contains("name the branch"), "{}", stderr(&out));
    // Inside the branch's harness, the branch is implied.
    let out = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .args(["workspace", "run", "dev"])
        .env("BRANCHYARD_BRANCH", "served")
        .output()
        .unwrap();
    assert!(
        stdout(&out).contains(&format!("serving on {port}")),
        "{}",
        stderr(&out)
    );
    let detached = repo.json(&["workspace", "run", "served", "--detach", "--json"]);
    let log = PathBuf::from(detached["log"].as_str().unwrap());
    for _ in 0..100 {
        if fs::read_to_string(&log).is_ok_and(|t| t.contains("serving")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(fs::read_to_string(&log)
        .unwrap()
        .contains(&format!("serving on {port}")));

    let events = repo.json(&["log", "served", "--json"]);
    let phases: Vec<(String, Value)> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["activity"] == "workspace")
        .map(|e| {
            (
                e["phase"].as_str().unwrap().to_owned(),
                e["exit_code"].clone(),
            )
        })
        .collect();
    let names: Vec<&str> = phases.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        names,
        ["copy", "setup", "run", "run", "run", "run"],
        "{phases:?}"
    );
    assert_eq!(phases[3].1, 4);
    let text = stdout(&repo.by(&["log", "served"]));
    assert!(text.contains("workspace run: exit 4"), "{text}");

    let out = repo.by(&["rm", "served"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("workspace teardown: ok"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        fs::read_to_string(repo.dir.join("teardown.log")).unwrap(),
        "served\n"
    );

    let out = repo.by_agent(&["run", "WRITE y.txt=1", "--name", "merged"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let out = repo.by(&["merge", "merged", "--rm"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("merged merged into main"));
    assert!(stdout(&out).contains("removed merged"));
    assert_eq!(
        fs::read_to_string(repo.dir.join("teardown.log")).unwrap(),
        "served\nmerged\n"
    );
    assert_eq!(repo.branches(), 0);
    assert!(repo.root.join("y.txt").is_file());
}

/// A run script's list runs entry by entry, each with its own `sh -c`, in
/// the foreground and detached alike: a `#` comment or a trailing `&` in
/// one entry means what it would alone, and the first failure stops it.
#[test]
fn run_script_entries_each_run_on_their_own() {
    let repo = Repo::new(
        r#"
[workspace]
setup = "true"

[workspace.run.commented]
command = ["echo ready # note", "echo second"]

[workspace.run.background]
command = ["sleep 0 &", "echo after-background"]

[workspace.run.stops]
command = ["echo one", "exit 3", "echo never"]
"#,
    );
    assert!(repo.by(&["workspace", "trust"]).status.success());
    let out = repo.by_agent(&["run", "WRITE x.txt=1", "--name", "listed"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let out = repo.by(&["workspace", "run", "listed", "commented"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "ready\nsecond\n");
    let out = repo.by(&["workspace", "run", "listed", "background"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "after-background\n");
    let out = repo.by(&["workspace", "run", "listed", "stops"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout(&out), "one\n");

    let events = repo.json(&["log", "listed", "--json"]);
    let runs: Vec<&Value> = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["activity"] == "workspace" && e["phase"] == "run")
        .collect();
    assert_eq!(runs.len(), 3);
    assert_eq!(
        runs[0]["commands"],
        json!(["echo ready # note", "echo second"])
    );
    assert_eq!(runs[2]["commands"], json!(["echo one", "exit 3"]));
    assert_eq!(runs[2]["exit_code"], 3);

    for (name, last, expected) in [
        ("commented", "second", "ready\nsecond\n"),
        ("background", "after-background", "after-background\n"),
    ] {
        let detached = repo.json(&["workspace", "run", "listed", name, "--detach", "--json"]);
        let log = PathBuf::from(detached["log"].as_str().unwrap());
        for _ in 0..250 {
            if fs::read_to_string(&log).is_ok_and(|t| t.contains(last)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(fs::read_to_string(&log).unwrap(), expected, "{name}");
    }
}

/// On a terminal, `by run` asks once and remembers a yes.
#[cfg(target_os = "linux")]
#[test]
fn a_terminal_is_asked_once_and_a_yes_is_remembered() {
    if !Path::new("/usr/bin/script").exists() {
        eprintln!("skipped: no /usr/bin/script for a pseudo-terminal");
        return;
    }
    let repo = Repo::new(WORKSPACE);
    let agent = fake_agent().display().to_string();
    let line = format!(
        "{} run 'WRITE x.txt=1' --name asked --yes --harness gemini-cli --command {agent}",
        env!("CARGO_BIN_EXE_by")
    );
    let mut process = repo
        .command("/usr/bin/script")
        .args(["-qefc", &line, "/dev/null"])
        .env("TERM", "xterm")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    process.stdin.take().unwrap().write_all(b"y\r").unwrap();
    let out = process.wait_with_output().unwrap();
    let screen = stdout(&out);
    assert!(out.status.success(), "{screen}");
    assert!(screen.contains("trust these scripts for"), "{screen}");
    assert!(
        screen.contains("setup     echo $BRANCHYARD_PORT"),
        "{screen}"
    );
    assert!(screen.contains("workspace setup: ok"), "{screen}");
    assert_eq!(
        repo.json(&["workspace", "show", "--json"])["trust"],
        "trusted"
    );
    // Remembered: no terminal needed now.
    let out = repo.by_agent(&["run", "WRITE y.txt=1", "--name", "again"]);
    assert!(out.status.success(), "{}", stderr(&out));

    // A no refuses, and creates nothing.
    fs::write(
        repo.root.join("branchyard.toml"),
        WORKSPACE.replace("exit 4", "exit 5"),
    )
    .unwrap();
    let line = line.replace("--name asked", "--name refused");
    let mut process = repo
        .command("/usr/bin/script")
        .args(["-qefc", &line, "/dev/null"])
        .env("TERM", "xterm")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    process.stdin.take().unwrap().write_all(b"n\r").unwrap();
    let out = process.wait_with_output().unwrap();
    let screen = stdout(&out);
    assert!(!out.status.success(), "{screen}");
    assert!(
        screen.contains("changed since you last trusted it"),
        "{screen}"
    );
    assert!(
        screen.contains("not trusted; nothing was created"),
        "{screen}"
    );
    assert_eq!(repo.branches(), 2);
}
