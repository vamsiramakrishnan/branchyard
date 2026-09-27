//! The built `by` binary end to end, against a temporary repository and the
//! fake ACP agent from branchyard-runtime. Hermetic: no real harness, no
//! network. Requires `git` and `sh`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Built once per test binary; cargo exposes a binary's path only to its
/// own package's tests.
fn fake_agent() -> &'static Path {
    static AGENT: OnceLock<PathBuf> = OnceLock::new();
    AGENT.get_or_init(|| {
        let by = PathBuf::from(env!("CARGO_BIN_EXE_by"));
        // target/<profile>/by
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
        assert!(
            command.status().unwrap().success(),
            "building fake-acp-agent failed"
        );
        let agent = profile_dir.join("fake-acp-agent");
        assert!(agent.is_file());
        agent
    })
}

struct Repo {
    dir: PathBuf,
    root: PathBuf,
}

impl Repo {
    fn new() -> Repo {
        let dir = std::env::temp_dir().join(format!(
            "branchyard-cli-test-{}-{}",
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
        repo.git(&["add", "."]);
        repo.git(&["commit", "-q", "-m", "initial"]);
        repo
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env("PAGER", "cat");
        // Never inherit a delegating harness's identity from whoever runs
        // the tests.
        for var in [
            "BRANCHYARD_DELEGATION",
            "BRANCHYARD_BRANCH",
            "BRANCHYARD_ROOT",
            "BRANCHYARD_BY",
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
            .output()
            .unwrap()
    }

    /// `by <args>` with the fake agent as the gemini-cli harness.
    fn by_agent(&self, args: &[&str]) -> Output {
        let agent = fake_agent().display().to_string();
        let mut all: Vec<&str> = args.to_vec();
        all.extend(["--command", &agent]);
        if args[0] != "send" && !args.contains(&"--harness") {
            all.extend(["--harness", "gemini-cli"]);
        }
        self.by(&all)
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.by(args);
        assert!(out.status.success(), "{}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
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
fn run_ls_diff_log_merge_and_rm() {
    let repo = Repo::new();
    let out = repo.by_agent(&["run", "WRITE hello.txt=hi", "--name", "hello"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    assert!(stderr(&out).contains("no isolation"), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("wrote hello.txt"), "{text}");
    assert!(
        text.contains("by/hello") && text.contains("ready"),
        "{text}"
    );

    let list = repo.json(&["ls", "--json"]);
    let branches = list.as_array().unwrap();
    assert_eq!(branches.len(), 1);
    assert_eq!(branches[0]["name"], "hello");
    assert_eq!(branches[0]["status"]["state"], "ready");
    assert_eq!(branches[0]["candidate"]["files_changed"], 1);
    assert_eq!(branches[0]["profile"], "gemini-cli-acp");

    let diff = repo.by(&["diff", "hello"]);
    assert!(stdout(&diff).contains("+hi"), "{}", stdout(&diff));

    let log = stdout(&repo.by(&["log", "hello"]));
    for expected in [
        "status: running",
        "harness ready",
        "prompt: WRITE hello.txt=hi",
        "wrote hello.txt",
        "candidate ",
        "session closed",
        "status: ready",
    ] {
        assert!(log.contains(expected), "{expected:?} missing from\n{log}");
    }
    let events = repo.json(&["log", "hello", "--json"]);
    let events = events.as_array().unwrap();
    assert_eq!(events[0]["activity"], "status");
    assert!(events.iter().any(|e| e["event"]["type"] == "turn_ended"));

    let merged = repo.by(&["merge", "hello"]);
    assert!(merged.status.success(), "{}", stderr(&merged));
    assert!(
        stdout(&merged).starts_with("merged hello into main"),
        "{}",
        stdout(&merged)
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("hello.txt")).unwrap(),
        "hi\n"
    );
    assert_eq!(
        repo.json(&["show", "hello", "--json"])["status"]["state"],
        "merged"
    );

    let removed = repo.by(&["rm", "hello"]);
    assert_eq!(stdout(&removed), "removed hello\n");
    assert_eq!(repo.json(&["ls", "--json"]), Value::Array(Vec::new()));
    assert_eq!(
        repo.git(&["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
}

#[test]
fn without_a_terminal_permissions_are_denied_and_shown() {
    let repo = Repo::new();
    let out = repo.by_agent(&["run", "PERMISSION WRITE p.txt=1", "--name", "p"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("will be denied"), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("denied write marker: Denied by Branchyard policy."),
        "{text}"
    );
    let log = stdout(&repo.by(&["log", "p"]));
    assert!(log.contains("denied write marker"), "{log}");

    let yes = repo.by_agent(&["run", "PERMISSION WRITE p.txt=1", "--name", "q", "--yes"]);
    assert!(
        stdout(&yes).contains("allowed write marker"),
        "{}",
        stdout(&yes)
    );
    let events = repo.json(&["log", "q", "--json"]);
    let decision = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["activity"] == "decision")
        .unwrap()
        .clone();
    assert_eq!(decision["allowed"], true);
    assert_eq!(decision["source"]["kind"], "default");
}

#[test]
fn fan_prefixes_branches_and_compares_them() {
    let repo = Repo::new();
    let out = repo.by_agent(&[
        "fan",
        "WRITE f.txt=x",
        "--harness",
        "gemini-cli,qwen-code",
        "--yes",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("write-f-txt-x-gemini-cli │ wrote f.txt"),
        "{text}"
    );
    assert!(
        text.contains("write-f-txt-x-qwen-code  │ wrote f.txt"),
        "{text}"
    );
    assert!(text.contains("by diff write-f-txt-x-qwen-code"), "{text}");
    assert_eq!(repo.json(&["ls", "--json"]).as_array().unwrap().len(), 2);
}

#[test]
fn send_continues_and_max_minutes_interrupts() {
    let repo = Repo::new();
    assert!(repo
        .by_agent(&["run", "WHOAMI", "--name", "s"])
        .status
        .success());
    let sent = repo.by_agent(&["send", "s", "WHOAMI"]);
    assert!(stdout(&sent).contains("resumed=true"), "{}", stdout(&sent));

    let hung = repo.by_agent(&["run", "HANG", "--name", "h", "--max-minutes", "0.005"]);
    assert!(hung.status.success(), "{}", stderr(&hung));
    assert!(
        stdout(&hung).contains("over budget: max_duration"),
        "{}",
        stdout(&hung)
    );
}

#[test]
fn errors_exit_nonzero() {
    let repo = Repo::new();
    let missing = repo.by(&["merge", "nope"]);
    assert_eq!(missing.status.code(), Some(1));
    assert_eq!(stderr(&missing), "by: no branch named nope\n");

    let usage = repo.by(&["run"]);
    assert_eq!(usage.status.code(), Some(2));

    let unknown = repo.by(&["run", "x", "--harness", "nope"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(stderr(&unknown).contains("no harness or profile named nope"));

    let absent = repo.by(&[
        "run",
        "x",
        "--harness",
        "gemini-cli",
        "--command",
        "/nonexistent/agent",
    ]);
    assert_eq!(absent.status.code(), Some(1));
    assert!(
        stderr(&absent).contains("unavailable"),
        "{}",
        stderr(&absent)
    );

    let failed = repo.by_agent(&["run", "EXIT", "--name", "exits"]);
    assert_eq!(failed.status.code(), Some(1), "{}", stdout(&failed));
    assert!(stdout(&failed).contains("failed"), "{}", stdout(&failed));

    let outside = Command::new(env!("CARGO_BIN_EXE_by"))
        .args(["ls"])
        .current_dir(&repo.dir)
        .output()
        .unwrap();
    assert_eq!(outside.status.code(), Some(1));
    assert!(stderr(&outside).contains("not inside a git work tree"));
}

/// The JSON value a `SH ... --json` line printed in a harness's reply,
/// after the `sh: <status>` line for the `n`th command.
fn sh_json(reply: &str, n: usize) -> (i32, Value) {
    let mut parts = reply.split("sh: ").skip(1 + n);
    let part = parts
        .next()
        .unwrap_or_else(|| panic!("no command {n} in {reply}"));
    let (status, rest) = part.split_once('\n').unwrap();
    let mut values = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
    (status.parse().unwrap(), values.next().unwrap().unwrap())
}

/// The harness's reply on `branch`'s last turn.
fn reply(repo: &Repo, branch: &str) -> String {
    repo.json(&["log", branch, "--json"])
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["event"]["type"] == "message_delta")
        .map(|e| e["event"]["text"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_harness_delegates_with_by_in_its_shell() {
    let repo = Repo::new();
    let prompt = [
        "SH by inspect --json",
        "SH by spawn 'WRITE kid.txt=k' --name kid --json",
        "SH by spawn 'WRITE later.txt=l' --name later --wait --json",
        "SH by children --json",
        "SH by inspect main --json",
        "SH by integrate later --json",
        "SH by send kid 'say more' --json",
        "SH by spawn x --parent other",
    ]
    .join("\n");
    let out = repo.by_agent(&["run", &prompt, "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    let (code, me) = sh_json(&said, 0);
    assert_eq!((code, me["name"].as_str()), (0, Some("root")));
    assert_eq!(me["envelope"]["max_depth"], 1);
    let (code, kid) = sh_json(&said, 1);
    assert_eq!((code, kid["name"].as_str()), (0, Some("kid")));
    assert_eq!(kid["status"]["state"], "running");
    let (code, later) = sh_json(&said, 2);
    assert_eq!(
        (code, later["status"]["state"].as_str()),
        (0, Some("ready"))
    );
    let (_, children) = sh_json(&said, 3);
    let names: Vec<&str> = children["descendants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["kid", "later"]);
    let (code, refused) = sh_json(&said, 4);
    assert_eq!(code, 1);
    assert_eq!(refused["error"]["kind"], "denied");
    assert!(refused["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not a descendant"));
    let (code, merged) = sh_json(&said, 5);
    assert_eq!((code, merged["target"].as_str()), (0, Some("by/root")));
    let (code, sent) = sh_json(&said, 6);
    // kid may still be running its first turn; either way the answer says so.
    assert!(
        (code == 0 && sent["name"] == "kid") || sent["error"]["kind"] == "running",
        "{said}"
    );
    assert!(said.contains("inside a harness, the parent is"), "{said}");

    // `by run` waited for the whole subtree, and `by ls` shows the tree.
    let text = stdout(&out);
    assert!(text.contains("delegated"), "{text}");
    let ls = stdout(&repo.by(&["ls"]));
    assert!(ls.contains("└ kid") && ls.contains("└ later"), "{ls}");
    let list = repo.json(&["ls", "--json"]);
    for branch in list.as_array().unwrap() {
        assert_ne!(branch["status"]["state"], "running", "{branch}");
    }
    let root = repo.json(&["show", "root", "--json"]);
    assert_eq!(root["children"], serde_json::json!(["kid", "later"]));
    assert_eq!(repo.json(&["show", "kid", "--json"])["depth"], 1);
}

#[test]
fn a_harness_delegates_with_the_python_module() {
    let repo = Repo::new();
    let script = "import branchyard as b; c = b.spawn('WRITE py.txt=p', name='py'); \
                  print('spawned', c.name, c.status['state']); d = b.wait(c.name, timeout=60, poll=0.05); \
                  print('finished', d.status['state'], d.candidate['files_changed']); \
                  print('merged into', b.integrate(c.name).target); \
                  print('children', [x['name'] for x in b.children().descendants]); \
                  exec('try:\\n b.inspect(\\'main\\')\\nexcept b.DeniedError as e:\\n print(\\'denied\\', e.kind)')";
    let prompt = format!("SH python3 -c \"{script}\"");
    let out = repo.by_agent(&["run", &prompt, "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let said = reply(&repo, "root");
    for expected in [
        "sh: 0",
        "spawned py running",
        "finished ready 1",
        "merged into by/root",
        "children ['py']",
        "denied denied",
    ] {
        assert!(said.contains(expected), "{expected:?} missing from\n{said}");
    }
    let root = repo.json(&["show", "root", "--json"]);
    assert_eq!(root["status"]["state"], "ready");
    assert_eq!(root["candidate"]["files_changed"], 1);
}

#[test]
fn the_same_commands_act_with_your_authority_outside_a_harness() {
    let repo = Repo::new();
    let out = repo.by_agent(&["run", "say hi", "--name", "root", "--delegate=2", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let plain = repo.by_agent(&["run", "say hi", "--name", "plain", "--yes"]);
    assert!(plain.status.success(), "{}", stderr(&plain));

    // Outside a harness, spawn names its parent and waits for the child.
    let agent = fake_agent().display().to_string();
    let spawned = repo.by(&[
        "spawn",
        "WRITE kid.txt=k",
        "--parent",
        "root",
        "--name",
        "kid",
        "--yes",
        "--json",
    ]);
    assert!(
        spawned.status.success(),
        "{}\n{}",
        stdout(&spawned),
        stderr(&spawned)
    );
    let kid: Value = serde_json::from_slice(&spawned.stdout).unwrap();
    assert_eq!(kid["status"]["state"], "ready");
    assert_eq!(kid["envelope"]["max_depth"], 1);
    assert_eq!(repo.json(&["inspect", "kid", "--json"])["parent"], "root");
    let children = repo.json(&["children", "root", "--json"]);
    assert_eq!(children["descendants"][0]["name"], "kid");
    let events = repo.json(&["events", "kid", "--cursor", "0", "--limit", "2", "--json"]);
    assert_eq!(events["next_cursor"], 2);
    assert_eq!(
        repo.json(&["cancel", "kid", "--json"]),
        serde_json::json!({"cancelled": []})
    );
    let merged = repo.json(&["integrate", "kid", "--json"]);
    assert_eq!(merged["target"], "by/root");
    assert_eq!(repo.git(&["show", "by/root:kid.txt"]), "k\n");

    // The envelope binds you too, and refusals are JSON with --json.
    let refused = repo.by(&["spawn", "x", "--parent", "plain", "--json"]);
    assert_eq!(refused.status.code(), Some(1));
    let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(refused["error"]["kind"], "denied");
    let text = repo.by(&["integrate", "plain"]);
    assert!(
        stderr(&text).contains("not delegated by another branch"),
        "{}",
        stderr(&text)
    );
    let inspect = stdout(&repo.by(&["inspect", "root"]));
    assert!(
        inspect.contains("children") && inspect.contains("kid"),
        "{inspect}"
    );
    let _ = agent;
}

#[test]
fn a_delegating_harness_gets_tools_and_skill_outside_its_worktree() {
    let repo = Repo::new();
    let prompt = "INSTRUCTED";
    let out = repo.by_agent(&["run", prompt, "--name", "root", "--delegate", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(reply(&repo, "root"), "instructed=true");
    let root = repo.json(&["show", "root", "--json"]);
    assert_eq!(root["status"]["state"], "no_changes");
    assert_eq!(root["candidate"], Value::Null);
    let worktree = root["worktree"].as_str().unwrap();
    let status = repo
        .command("git")
        .args([
            "-C",
            worktree,
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignored",
        ])
        .output()
        .unwrap();
    assert_eq!(stdout(&status), "", "the worktree is untouched");
    let dir = repo.root.join(".branchyard");
    assert!(dir.join("plugin/skills/delegate/SKILL.md").is_file());
    assert!(dir.join("plugin/.claude-plugin/plugin.json").is_file());
    assert!(dir.join("sdk/python/branchyard.py").is_file());
    assert_eq!(
        repo.git(&["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
    // Without delegation there is no preamble, and `by mcp` is the server
    // when there is.
    let plain = repo.by_agent(&["run", "INSTRUCTED", "--name", "plain", "--yes"]);
    assert!(plain.status.success());
    assert_eq!(reply(&repo, "plain"), "instructed=false");
    let tools = repo.by_agent(&["run", "MCP tools", "--name", "mcp", "--delegate", "--yes"]);
    assert!(tools.status.success(), "{}", stderr(&tools));
    assert!(
        reply(&repo, "mcp")
            .contains("mcp tools: spawn,inspect,events,send,propose_integration,cancel,children"),
        "{}",
        reply(&repo, "mcp")
    );
    // Inside a harness that was not given delegation, `by` will not act
    // with your authority.
    let by = env!("CARGO_BIN_EXE_by");
    let bare = repo.by_agent(&[
        "run",
        &format!("SH {by} spawn x --parent root --json"),
        "--name",
        "bare",
        "--yes",
    ]);
    assert!(bare.status.success());
    let (code, refused) = sh_json(&reply(&repo, "bare"), 0);
    assert_eq!(code, 1);
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("was not given delegation"),
        "{refused}"
    );
}

#[test]
fn by_mcp_needs_a_token() {
    let repo = Repo::new();
    let out = repo.by(&["mcp", "--root", "/tmp", "--branch", "b"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("BRANCHYARD_DELEGATION is not set"),
        "{}",
        stderr(&out)
    );
}
