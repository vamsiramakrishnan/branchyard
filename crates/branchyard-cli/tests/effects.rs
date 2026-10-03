//! `by approvals`, `by effects`, `by undo` and `by rewind`'s upstream plan,
//! with the built `by` against a mock gateway on loopback
//! (`mock_gateway`): a granted turn's calls go through its ledger proxy,
//! an ask is answered from another `by`, a killed engine leaves an entry
//! unknown that `by effects reconcile` settles through its lookup, and an
//! undo rewinds the files and performs the chosen inverse. Hermetic.

#[path = "../../branchyard/tests/mock_gateway/mod.rs"]
mod mock_gateway;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use mock_gateway::MockGateway;
use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Anvil's packaging commands, as far as Branchyard calls them.
const FAKE_ANVIL: &str = r##"#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
def flag(name):
    return args[args.index(name) + 1]
if args[:2] == ["package", "harness"]:
    out = flag("--out")
    os.makedirs(out, exist_ok=True)
    with open(os.path.join(out, "SKILL.md"), "w") as f:
        f.write("# " + os.path.basename(args[2]) + "\n")
elif args[:2] == ["connectors", "index"]:
    with open(flag("--out"), "w") as f:
        f.write("# Connectors\n")
else:
    sys.exit("fake anvil: " + " ".join(args))
"##;

const BUNDLES: [&str; 4] = ["slack", "github", "gmail", "legacy"];

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
        assert!(
            command.status().unwrap().success(),
            "building fake-acp-agent failed"
        );
        profile_dir.join("fake-acp-agent")
    })
}

struct Repo {
    dir: PathBuf,
    root: PathBuf,
    mock: MockGateway,
    script: String,
}

impl Repo {
    /// A repository whose gateway is the mock, with `approvals` as its
    /// `[approvals]` table's lines.
    fn new(approvals: &str) -> Repo {
        let dir = std::env::temp_dir().join(format!(
            "branchyard-cli-effects-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let root = dir.join("repo");
        let mock = MockGateway::start();
        let script = dir.join("call.py");
        fs::write(&script, mock_gateway::CALL_PY).unwrap();
        let repo = Repo {
            root,
            mock,
            script: script.display().to_string(),
            dir,
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "user.email", "test@localhost"]);
        fs::write(repo.root.join("a.txt"), "one\n").unwrap();
        let anvil = repo.dir.join("fake-anvil");
        fs::write(&anvil, FAKE_ANVIL).unwrap();
        fs::set_permissions(&anvil, fs::Permissions::from_mode(0o755)).unwrap();
        let bundles = repo.dir.join("bundles");
        for id in BUNDLES {
            fs::create_dir_all(bundles.join(id)).unwrap();
            fs::write(
                bundles.join(id).join("air.yaml"),
                format!("service: {id}\n"),
            )
            .unwrap();
        }
        fs::write(
            repo.root.join("branchyard.toml"),
            format!(
                "[connectors]\ngateway = \"{}\"\nbundles = \"{}\"\nanvil = \"{}\"\n\n[approvals]\n{approvals}\n",
                repo.mock.url,
                bundles.display(),
                anvil.display()
            ),
        )
        .unwrap();
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
            .env("USER", "ana")
            .env(
                "BRANCHYARD_USER_CONFIG",
                "/nonexistent/branchyard-config.toml",
            );
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
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.by(args);
        assert!(
            out.status.success(),
            "{args:?}: {}\n{}",
            stdout(&out),
            stderr(&out)
        );
        stdout(&out)
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.ok(args)).unwrap()
    }

    /// `by run` of `prompt` as `name`, granted every bundle, with the fake
    /// agent.
    fn run_args(&self, prompt: &str, name: &str) -> Vec<String> {
        let mut args: Vec<String> = ["run", prompt, "--name", name, "--isolated"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        for id in BUNDLES {
            args.push("--connector".into());
            args.push(format!("{id}:write"));
        }
        args.extend(
            [
                "--harness",
                "gemini-cli",
                "--command",
                &fake_agent().display().to_string(),
                "--yes",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        args
    }

    fn spawn_run(&self, prompt: &str, name: &str) -> Child {
        self.command(env!("CARGO_BIN_EXE_by"))
            .args(self.run_args(prompt, name))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn call(&self, tool: &str, arguments: &str) -> String {
        format!("SH python3 {} {tool} '{arguments}'", self.script)
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

fn python() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Wait for `ready`, at most a minute.
fn until<T>(what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(found) = ready() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn effects_are_listed_and_undo_plans_then_rewinds_and_undoes() {
    if !python() {
        eprintln!("skipped: python3 is not installed");
        return;
    }
    let repo = Repo::new("rules = { \"legacy:*\" = \"allow\" }");
    repo.mock.set_deadline(4_102_444_800_000);
    let first = format!(
        "SH echo 1 > first.txt\n{}",
        repo.call("slack__chat_post", r#"{"channel": "general"}"#)
    );
    let out = repo.by(&repo
        .run_args(&first, "board")
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>());
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    let second = format!(
        "SH echo 2 > second.txt\n{}\n{}\n{}",
        repo.call("slack__chat_post", r#"{"channel": "board"}"#),
        repo.call("legacy__do", "{}"),
        repo.call("gmail__send", r#"{"to": "finance@"}"#),
    );
    repo.ok(&["send", "board", &second, "--yes"]);

    // The ledger.
    let ledger = repo.json(&["effects", "--json"]);
    let entries = ledger.as_array().unwrap();
    assert_eq!(entries.len(), 4, "{ledger}");
    let states: Vec<&str> = entries
        .iter()
        .map(|e| e["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["confirmed", "confirmed", "confirmed", "staged"]);
    let table = repo.ok(&["effects", "--branch", "board"]);
    assert!(table.starts_with("ID "), "{table}");
    assert!(table.contains("slack      chat_post"), "{table}");
    assert!(table.contains("slack.chat.delete until"), "{table}");
    let show = repo.ok(&["show", "board"]);
    assert!(show.contains("effects"), "{show}");
    assert!(
        show.contains("4 in the ledger (3 confirmed, 1 staged"),
        "{show}"
    );

    // The plan, as the person reads it.
    let plan = repo.ok(&["undo", "board", "--to", "1", "--plan"]);
    assert!(
        plan.starts_with("Rewinding \"board\" to turn 1:\n"),
        "{plan}"
    );
    assert!(
        plan.contains("  files and conversation        restored exactly\n"),
        "{plan}"
    );
    assert!(
        plan.contains(
            "  upstream, can be undone       slack: chat_post (undone by slack.chat.delete"
        ),
        "{plan}"
    );
    assert_eq!(
        plan.matches("slack: chat_post").count(),
        1,
        "turn 1's post is kept: {plan}"
    );
    assert!(
        plan.contains("  upstream, cannot be undone    legacy: do"),
        "{plan}"
    );
    assert!(
        plan.contains("  upstream, staged              gmail: send (a draft, never performed: it is discarded with gmail.drafts.delete)"),
        "{plan}"
    );
    let json = repo.json(&["undo", "board", "--to", "1", "--plan", "--json"]);
    assert_eq!(json["reversible"].as_array().unwrap().len(), 1);
    assert_eq!(json["to"], 1);
    // Nothing changed.
    assert!(repo.mock.calls_to("slack__chat_delete").is_empty());

    // `by rewind` says what it leaves upstream.
    let rewound = repo.by(&["rewind", "board", "--to", "1", "--yes"]);
    assert!(rewound.status.success(), "{}", stderr(&rewound));
    assert!(
        stderr(&rewound).contains("by undo board --to 1"),
        "{}",
        stderr(&rewound)
    );

    // Undo without a terminal needs --yes or --only.
    let refused = repo.by(&["undo", "board", "--to", "1"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("pass --yes"),
        "{}",
        stderr(&refused)
    );
    // Undo: files back to turn 1, the post deleted, the draft discarded.
    let done = repo.json(&["undo", "board", "--to", "1", "--yes", "--json"]);
    let outcomes = done["outcomes"].as_array().unwrap();
    assert_eq!(outcomes.len(), 2, "{done}");
    assert!(outcomes.iter().any(|o| o["state"] == "undone"), "{done}");
    assert!(outcomes.iter().any(|o| o["state"] == "failed"), "{done}");
    assert_eq!(repo.mock.calls_to("slack__chat_delete").len(), 1);
    assert_eq!(repo.mock.calls_to("gmail__drafts_delete").len(), 1);
    let worktree = PathBuf::from(
        repo.json(&["show", "board", "--json"])["worktree"]
            .as_str()
            .unwrap(),
    );
    assert!(worktree.join("first.txt").is_file());
    assert!(!worktree.join("second.txt").exists());
    let after = repo.json(&["effects", "--json"]);
    let undone = after
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["state"] == "undone")
        .unwrap();
    assert_eq!(undone["undo_approval"]["by"], "ana");
    assert_eq!(undone["undo_approval"]["surface"], "cli");
    // A second undo finds nothing more it may do there.
    let again = repo.json(&["undo", "board", "--to", "1", "--plan", "--json"]);
    assert!(
        again["reversible"].as_array().unwrap().is_empty(),
        "{again}"
    );
}

#[test]
fn an_ask_is_answered_from_another_by_and_a_killed_engine_is_reconciled() {
    if !python() {
        eprintln!("skipped: python3 is not installed");
        return;
    }
    let repo = Repo::new("rules = { \"gmail:*\" = \"allow\" }");
    // A compensable call asks; another `by` answers it.
    let mut run = repo.spawn_run(
        &repo.call("github__issues_create", r#"{"title": "t"}"#),
        "asker",
    );
    let asks = until("an ask", || {
        let asks = repo.json(&["approvals", "--json"]);
        (!asks.as_array().unwrap().is_empty()).then_some(asks)
    });
    let ask = &asks[0];
    assert_eq!(ask["branch"], "asker");
    assert_eq!(ask["about"]["operation"], "issues_create");
    let listed = repo.ok(&["approvals"]);
    assert!(
        listed.contains("github issues_create (compensable) [default]"),
        "{listed}"
    );
    let id = ask["id"].as_str().unwrap();
    let answered = repo.ok(&[
        "approvals",
        "allow",
        &id[id.len() - 8..],
        "--reason",
        "fine",
    ]);
    assert!(answered.starts_with("allowed approval"), "{answered}");
    let out = run.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let entry = &repo.json(&["effects", "--branch", "asker", "--json"])[0];
    assert_eq!(entry["state"], "confirmed");
    assert_eq!(entry["approval"]["by"], "ana");
    assert_eq!(entry["approval"]["surface"], "cli");
    let log = repo.ok(&["log", "asker"]);
    assert!(log.contains("allowed by ana (cli): fine"), "{log}");
    // Answered once.
    let twice = repo.by(&["approvals", "deny", id]);
    assert!(!twice.status.success());
    assert!(
        stderr(&twice).contains("already allowed by ana"),
        "{}",
        stderr(&twice)
    );

    // The engine is killed after the gateway did the effect and before it
    // answered: the entry was begun before the call, so it is unknown,
    // then the lookup settles it.
    repo.mock.hold("gmail__send");
    run = repo.spawn_run(&repo.call("gmail__send", r#"{"to": "x@"}"#), "crash");
    repo.mock.wait_held();
    run.kill().unwrap();
    let _ = run.wait();
    repo.mock.release();
    let entry = &repo.json(&["effects", "--branch", "crash", "--json"])[0];
    assert_eq!(entry["state"], "unknown", "{entry}");
    let held = &repo.mock.calls_to("gmail__send")[0];
    assert_eq!(held.key.as_deref(), entry["id"].as_str());
    let reconciled = repo.ok(&["effects", "reconcile"]);
    assert!(reconciled.contains("confirmed"), "{reconciled}");
    let entry = &repo.json(&["effects", "--branch", "crash", "--json"])[0];
    assert_eq!(entry["state"], "confirmed");
    // Never called again.
    assert_eq!(repo.mock.calls_to("gmail__send").len(), 1);
    let shown = repo.ok(&["effects", "show", &entry["id"].as_str().unwrap()[18..]]);
    assert!(
        shown.contains("the lookup (gmail.sent.lookup) found it"),
        "{shown}"
    );
}
