//! `[workspace]` through the built `by`, against temporary repositories and
//! the fake ACP agent: the trust decision (refused without a terminal,
//! granted by `by workspace trust` or on a terminal, asked again after a
//! change, never needed for copy-only or your own user file, never taken
//! by a harness), `by workspace show` and `run`, and teardown on `by rm`
//! and `by merge --rm`. Hermetic; requires `git` and `sh`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use branchyard_testkit::fake_agent;
use branchyard_testkit::wait;
use serde_json::{json, Value};

/// The kit's repository, plus what this file adds.
struct Repo(branchyard_testkit::Repo);

impl std::ops::Deref for Repo {
    type Target = branchyard_testkit::Repo;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
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
        let mut kit = branchyard_testkit::repo!(&[
            ("a.txt", "one\n"),
            (".gitignore", ".env\nbranchyard.toml\n")
        ]);
        kit.remove_env("BRANCHYARD_TRUST_FILE");
        let repo = Repo(kit);
        fs::write(repo.root.join(".env"), "TOKEN_NAME=x\n").unwrap();
        fs::write(repo.root.join("branchyard.toml"), project).unwrap();
        repo
    }

    fn user_config(&self) -> PathBuf {
        self.dir.join("user/config.toml")
    }

    fn by_agent(&self, args: &[&str]) -> Output {
        let agent = fake_agent!().display().to_string();
        let mut all: Vec<&str> = args.to_vec();
        all.extend(["--command", &agent, "--yes"]);
        if args[0] != "send" {
            all.extend(["--harness", "gemini-cli"]);
        }
        self.by(&all)
    }

    fn branches(&self) -> usize {
        self.json(&["ls", "--json"]).as_array().unwrap().len()
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
    wait::until("the detached run to print", || {
        fs::read_to_string(&log).is_ok_and(|t| t.contains("serving"))
    });
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
        wait::until(&format!("the {name} run to print {last:?}"), || {
            fs::read_to_string(&log).is_ok_and(|t| t.contains(last))
        });
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
    let agent = fake_agent!().display().to_string();
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

#[test]
fn by_env_lists_shows_rebuilds_and_prunes_prepared_environments() {
    let repo = Repo::new(
        "[workspace]\nsetup = \"mkdir -p deps && echo lib > deps/lib.txt\"\nprepare = true\n\
         share = [\"deps\"]\n",
    );
    let out = repo.by(&["env", "list"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("No prepared environments yet"),
        "{}",
        stdout(&out)
    );
    // Rebuilding runs setup: it needs trust.
    let out = repo.by(&["env", "rebuild"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("by workspace trust"),
        "{}",
        stderr(&out)
    );
    assert!(repo.by(&["workspace", "trust"]).status.success());
    let shown = repo.json(&["env", "show", "--json"]);
    assert_eq!(shown["environment"], Value::Null);

    let out = repo.by(&["env", "rebuild"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("built environment"),
        "{}",
        stdout(&out)
    );
    let listed = repo.json(&["env", "list", "--json"]);
    let key = listed["current"].as_str().unwrap().to_owned();
    assert_eq!(listed["environments"][0]["key"], key.as_str());
    assert_eq!(listed["environments"][0]["state"], "good");
    assert_eq!(listed["environments"][0]["built_by"], "by env rebuild");
    let shown = repo.json(&["env", "show", &key[..8], "--json"]);
    assert_eq!(shown["produced"], json!(["deps"]));
    let out = repo.by(&["env", "show"]);
    assert!(stdout(&out).contains("produced  deps"), "{}", stdout(&out));

    // A new branch starts from it: setup does not run, the directory is
    // linked.
    let out = repo.by_agent(&["run", "WRITE x.txt=1", "--name", "a"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let worktree = PathBuf::from(
        repo.json(&["workspace", "show", "a", "--json"])["worktree"]
            .as_str()
            .unwrap(),
    );
    let link = fs::read_link(worktree.join("deps")).unwrap();
    assert!(
        link.ends_with(format!("environments/{key}/tree/deps")),
        "{link:?}"
    );
    let log = repo.json(&["log", "a", "--json"]);
    let restored = log
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["activity"] == "workspace" && e["environment"].is_object())
        .unwrap_or_else(|| panic!("{log}"));
    assert_eq!(restored["environment"]["origin"], "restored");

    // Linked: kept, even when named.
    let out = repo.by(&["env", "prune", &key]);
    assert!(stdout(&out).contains("linked by a"), "{}", stdout(&out));
    assert!(repo.by(&["rm", "a"]).status.success());
    let out = repo.by(&["env", "prune", &key, "--json"]);
    let pruned: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(pruned["removed"][0][0], key.as_str());
    assert!(repo.json(&["env", "list", "--json"])["environments"]
        .as_array()
        .unwrap()
        .is_empty());
    // Not on a server.
    let out = repo.by(&["--remote", "http://127.0.0.1:9", "env", "list"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn by_env_pool_fills_and_by_run_starts_from_a_ready_slot() {
    let repo = Repo::new(
        "[workspace]\nsetup = \"mkdir -p deps && echo lib > deps/lib.txt && \
         echo ran >> $BRANCHYARD_ROOT/../setups.log\"\nprepare = true\nshare = [\"deps\"]\n\n\
         [workspace.pool]\nsize = 1\n",
    );
    let setups = || {
        fs::read_to_string(repo.dir.join("setups.log"))
            .map(|t| t.lines().count())
            .unwrap_or(0)
    };
    let out = repo.by(&["env", "pool", "status"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("0 of 1 ready"), "{}", stdout(&out));
    // Filling may run setup: it needs trust, and never from a harness.
    let out = repo.by(&["env", "pool", "fill"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("by workspace trust"),
        "{}",
        stderr(&out)
    );
    assert!(repo.by(&["workspace", "trust"]).status.success());
    let out = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .args(["env", "pool", "fill"])
        .env("BRANCHYARD_BRANCH", "some-branch")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("a person's decision"),
        "{}",
        stderr(&out)
    );

    let filled = repo.json(&["env", "pool", "fill", "--json"]);
    assert_eq!(filled["ready"], 1, "{filled}");
    let slot = filled["made"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(setups(), 1);
    let status = repo.json(&["env", "pool", "status", "--json"]);
    assert_eq!(status["slots"][0]["id"], slot.as_str());
    assert_eq!(status["slots"][0]["state"], "ready");
    let out = repo.by(&["env", "pool", "status"]);
    assert!(stdout(&out).contains("1 of 1 ready"), "{}", stdout(&out));

    // `by run` takes the ready slot: its setup runs nothing, its worktree
    // is the slot's, the shared directory still a link.
    let out = repo.by_agent(&["run", "SH cat deps/lib.txt", "--name", "a"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(setups(), 1);
    let log = repo.json(&["log", "a", "--json"]);
    let setup = log
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["activity"] == "workspace" && e["pool"].is_object())
        .unwrap_or_else(|| panic!("{log}"));
    assert_eq!(setup["pool"]["slot"], slot.as_str(), "{setup}");
    assert_eq!(setup["environment"]["origin"], "restored");
    let out = repo.by(&["log", "a"]);
    assert!(
        stdout(&out).contains(&format!("worktree from warm pool slot {slot}")),
        "{}",
        stdout(&out)
    );
    let worktree = PathBuf::from(
        repo.json(&["workspace", "show", "a", "--json"])["worktree"]
            .as_str()
            .unwrap(),
    );
    assert!(fs::symlink_metadata(worktree.join("deps"))
        .unwrap()
        .file_type()
        .is_symlink());
    // The CLI does not refill: the next branch misses, and says why.
    let out = repo.by_agent(&["run", "SH cat deps/lib.txt", "--name", "b"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let stats = repo.json(&["stats", "--json"]);
    assert_eq!(stats["pool"]["hits"], 1, "{stats}");
    assert_eq!(stats["pool"]["misses"], 1, "{stats}");
    assert_eq!(stats["pool"]["ready"], 0, "{stats}");
    assert_eq!(stats["pool"]["size"], 1, "{stats}");
    assert_eq!(stats["pool"]["start_hit_seconds"]["count"], 1, "{stats}");
    let out = repo.by(&["stats"]);
    assert!(
        stdout(&out).contains("pool      1 hits, 1 misses; 0 of 1 ready"),
        "{}",
        stdout(&out)
    );

    // Drained: nothing left.
    assert!(repo.by(&["env", "pool", "fill"]).status.success());
    let drained = repo.json(&["env", "pool", "drain", "--json"]);
    assert_eq!(drained["removed"].as_array().unwrap().len(), 1, "{drained}");
    let status = repo.json(&["env", "pool", "status", "--json"]);
    assert!(status["slots"].as_array().unwrap().is_empty());
    let out = repo.by(&["--remote", "http://127.0.0.1:9", "env", "pool", "status"]);
    assert_eq!(out.status.code(), Some(1));
}
