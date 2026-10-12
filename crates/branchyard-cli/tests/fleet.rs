//! The fleet table through the built `by`, against temporary repositories
//! and the fake ACP agent: routing with a `[fleet]` and no `--harness`,
//! `--auto` failing over from a harness that exits, `--kind`, `by fan
//! --auto --judge` with a judge harness answering a canned verdict, `by
//! judge --pick`, `by fleet stats|route`, the inventory default `--auto`
//! routes by without a `[fleet]` (fake harnesses on a private `PATH`), and
//! the refusals. Hermetic: no model is called, and no harness of the
//! machine running the tests is found. Requires `git` and `sh`.

#![allow(clippy::expect_used, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;

use branchyard_testkit::fake_agent;
use serde_json::Value;

/// The kit's repository, plus what this file adds.
struct Repo(branchyard_testkit::Repo);

impl std::ops::Deref for Repo {
    type Target = branchyard_testkit::Repo;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Key variables a harness's login is read from, which the machine
/// running the tests may set: none reaches `by` unless a test sets it.
const KEY_VARIABLES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CONFIG_DIR",
    "CODEX_API_KEY",
    "CODEX_HOME",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "AGY_TOKEN",
];

impl Repo {
    /// A repository whose `by` finds only the harnesses a test fakes into
    /// `bin` ([`Repo::fake`]): `PATH` is `bin` and the system's tools, and
    /// `HOME` is a scratch directory.
    fn new() -> Repo {
        let mut repo =
            branchyard_testkit::repo!(&[("a.txt", "one\n"), (".gitignore", "branchyard.toml\n")]);
        let (bin, home) = (repo.dir.join("bin"), repo.dir.join("home"));
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(home.join(".local/bin")).unwrap();
        repo.set_env("PATH", &format!("{}:/usr/bin:/bin", bin.display()));
        repo.set_env("HOME", &home.display().to_string());
        repo.set_env("BRANCHYARD_HARNESS_DIRS", "$HOME/.local/bin");
        repo.set_env("BRANCHYARD_HARNESS_TIMEOUT", "2");
        for name in KEY_VARIABLES {
            repo.remove_env(name);
        }
        Repo(repo)
    }

    /// A fake harness executable `name` in `bin`, running `body` (sh).
    fn fake(&self, name: &str, body: &str) -> PathBuf {
        let path = self.dir.join("bin").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Write `branchyard.toml`, with `AGENT` replaced by the fake agent.
    fn config(&self, text: &str) {
        let agent = fake_agent!().display().to_string();
        fs::write(
            self.root.join("branchyard.toml"),
            text.replace("AGENT", &agent),
        )
        .unwrap();
    }

    /// A seed under which `by fleet route` picks `harness` first.
    fn seed_for(&self, prompt: &str, harness: &str) -> String {
        (0..500)
            .map(|seed| seed.to_string())
            .find(|seed| {
                let route = self.json(&["fleet", "route", prompt, "--seed", seed, "--json"]);
                route["picks"][0]["candidate"]["harness"] == harness
            })
            .expect("a seed picks it")
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

const TWO: &str = r#"
[fleet.default]
candidates = [
  { harness = "gemini-cli", command = "AGENT" },
  { harness = "qwen-code", command = "AGENT" },
]
"#;

#[test]
fn a_fleet_routes_a_run_that_names_no_harness_and_records_its_outcome() {
    let repo = Repo::new();
    repo.config(TWO);
    let out = repo.ok(&[
        "run",
        "Fix it WRITE fixed.txt=1",
        "-n",
        "routed",
        "--seed",
        "4",
        "--yes",
    ]);
    let err = stderr(&out);
    assert!(err.contains("routed as bugfix (classifier: fix"), "{err}");
    assert!(err.contains("by [fleet.default], seed 4"), "{err}");
    let show = repo.json(&["show", "routed", "--json"]);
    assert_eq!(show["status"]["state"], "ready", "{show}");
    let log = repo.json(&["log", "routed", "--json"]);
    let fleet: Vec<&Value> = log
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["activity"] == "fleet")
        .collect();
    assert_eq!(fleet[0]["fleet"]["type"], "routed", "{log}");
    assert_eq!(fleet[0]["fleet"]["kind"], "bugfix");
    let text = stdout(&repo.ok(&["log", "routed"]));
    assert!(
        text.contains("routed bugfix (classifier) by [fleet.default]"),
        "{text}"
    );

    let stats = repo.json(&["fleet", "stats", "--json"]);
    assert_eq!(stats.as_array().unwrap().len(), 1, "{stats}");
    assert_eq!(stats[0]["kind"], "bugfix");
    assert_eq!(stats[0]["ready"], 1);
    let table = stdout(&repo.ok(&["fleet", "stats"]));
    assert!(table.lines().next().unwrap().starts_with("KIND"), "{table}");
    assert!(table.contains("bugfix"), "{table}");

    // A named harness is not routed; --kind is still recorded.
    let agent = fake_agent!().display().to_string();
    repo.ok(&[
        "run",
        "WRITE d.txt=1",
        "-n",
        "named",
        "--harness",
        "gemini-cli",
        "--command",
        &agent,
        "--kind",
        "docs",
        "--yes",
    ]);
    let docs = repo.json(&["fleet", "stats", "--kind", "docs", "--json"]);
    assert_eq!(docs[0]["harness"], "gemini-cli", "{docs}");
    assert_eq!(docs.as_array().unwrap().len(), 1);
}

#[test]
fn auto_fails_over_when_a_harness_exits() {
    let repo = Repo::new();
    repo.config(
        r#"
[fleet.default]
candidates = [
  { harness = "gemini-cli", command = "/bin/false" },
  { harness = "qwen-code", command = "AGENT" },
]
exploration = 0
"#,
    );
    let prompt = "Add a file WRITE added.txt=1";
    let seed = repo.seed_for(prompt, "gemini-cli");
    let out = repo.ok(&[
        "run", prompt, "--auto", "-n", "first", "--seed", &seed, "--yes",
    ]);
    let err = stderr(&out);
    assert!(
        err.contains("first's harness failed; the task went on as"),
        "{err}"
    );
    let ls = repo.json(&["ls", "--json"]);
    let next = ls
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["parent"] == "first")
        .unwrap_or_else(|| panic!("{ls}"));
    assert_eq!(next["harness"], "qwen-code");
    assert_eq!(next["status"]["state"], "ready");
    let log = stdout(&repo.ok(&["log", "first"]));
    assert!(
        log.contains("failing over to qwen-code: the harness exited"),
        "{log}"
    );
}

#[test]
fn a_routed_fan_is_judged_by_a_harness_and_the_pick_merges() {
    let repo = Repo::new();
    let verdict = repo.dir.join("verdict.json");
    fs::write(
        &verdict,
        r#"{"ranking": ["trio-qwen-code", "trio-gemini-cli"],
            "scores": {"trio-qwen-code": 91, "trio-gemini-cli": 55},
            "reasons": {"trio-qwen-code": "clean", "trio-gemini-cli": "fine"}}"#,
    )
    .unwrap();
    repo.config(&format!(
        r#"{TWO}attempts = 2
judge = {{ harness = "gemini-cli", command = "AGENT", rubric = "REPLY_FILE {}" }}
"#,
        verdict.display()
    ));
    let out = repo.ok(&[
        "fan",
        "WRITE f.txt=1",
        "--auto",
        "-n",
        "trio",
        "--judge",
        "--seed",
        "1",
        "--yes",
    ]);
    let text = stdout(&out);
    assert!(text.contains("judged by harness gemini-cli"), "{text}");
    assert!(text.contains("proposed pick: trio-qwen-code"), "{text}");
    assert!(
        text.lines()
            .any(|l| l.contains("trio-qwen-code") && l.contains("91")),
        "{text}"
    );
    // The judge's scratch branch is gone.
    assert_eq!(repo.json(&["ls", "--json"]).as_array().unwrap().len(), 2);

    // Deterministic, then with the judge again and --pick.
    let det = repo.json(&["judge", "trio", "--deterministic", "--json"]);
    assert_eq!(det["by"]["by"], "deterministic", "{det}");
    assert_eq!(det["candidates"].as_array().unwrap().len(), 2);
    let picked = repo.json(&[
        "judge",
        "trio",
        "--pick",
        "--discard-others",
        "--yes",
        "--json",
    ]);
    assert_eq!(picked["picked"], "trio-qwen-code", "{picked}");
    assert_eq!(picked["removed"][0], "trio-gemini-cli");
    assert!(
        repo.root.join("f.txt").is_file(),
        "the pick merged into main"
    );
    let stats = repo.json(&["fleet", "stats", "--json"]);
    let qwen = stats
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["harness"] == "qwen-code")
        .unwrap();
    assert_eq!(qwen["merged"], 1, "{stats}");
}

#[test]
fn routing_is_refused_with_reasons() {
    let repo = Repo::new();
    // --auto without a [fleet], and no harness logged in here.
    let out = repo.by(&["run", "x", "--auto", "--yes"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains(
            "with no [fleet], --auto routes among the harnesses logged in here, and none is \
             installed"
        ),
        "{}",
        stderr(&out)
    );
    let out = repo.by(&["fan", "x", "--auto", "--yes"]);
    assert!(
        stderr(&out).contains("and none is installed"),
        "{}",
        stderr(&out)
    );
    // fan with neither --harness nor a [fleet].
    let out = repo.by(&["fan", "x", "--yes"]);
    assert!(
        stderr(&out).contains("fan needs --harness"),
        "{}",
        stderr(&out)
    );
    // --auto and --harness together.
    let out = repo.by(&["run", "x", "--auto", "--harness", "codex"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    // An unknown kind.
    let out = repo.by(&["run", "x", "--kind", "chores"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("not a task kind"), "{}", stderr(&out));
    // A strict table.
    repo.config("[fleet.default]\ncandidates = [{ harness = \"gemini-cli\", modle = \"x\" }]\n");
    let out = repo.by(&["config", "validate"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("modle") || stdout(&out).contains("modle"));
    repo.config("[fleet.default]\ncandidates = [{ harness = \"branchyard-none\" }]\n");
    let out = repo.by(&["run", "x", "--yes"]);
    assert!(!out.status.success());
    // Every candidate unavailable.
    repo.config(
        "[fleet.default]\ncandidates = [{ harness = \"gemini-cli\", command = \"branchyard-no-such\" }]\n",
    );
    let out = repo.by(&["run", "x", "--yes"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no candidate can run"),
        "{}",
        stderr(&out)
    );
    assert!(repo.json(&["ls", "--json"]).as_array().unwrap().is_empty());
    let route = repo.by(&["fleet", "route", "x"]);
    assert!(!route.status.success());
}

/// Without a `[fleet]`, `--auto` and `by fleet route` route among the
/// harnesses logged in on this machine at that moment: Codex (verified by
/// its status command) and Gemini CLI (by `GEMINI_API_KEY`); Claude Code,
/// installed but logged out, and Goose, whose login cannot be told, are
/// listed as left out. `[defaults] harness` goes first, and a `[fleet]`
/// turns the default off.
#[test]
fn without_a_fleet_auto_routes_among_the_harnesses_logged_in_here() {
    let mut repo = Repo::new();
    let agent = fake_agent!().display().to_string();
    repo.fake("claude", "echo '2.1.283 (Claude Code)'");
    repo.fake(
        "codex",
        r#"case "$1" in --version) echo "codex-cli 0.157.1";; login) echo "Logged in using ChatGPT";; *) exit 3;; esac"#,
    );
    repo.fake(
        "gemini",
        &format!("[ \"$1\" = --version ] && {{ echo 0.63.0; exit 0; }}\nexec '{agent}'"),
    );
    repo.fake("goose", "echo 'goose 1.0.0'");
    repo.0.set_env("GEMINI_API_KEY", "not-a-key");

    let route = repo.json(&["fleet", "route", "Fix the bug", "--seed", "1", "--json"]);
    assert_eq!(route["entry"], "inventory", "{route}");
    let picked = route["picks"][0]["candidate"]["harness"].as_str().unwrap();
    assert!(["codex", "gemini-cli"].contains(&picked), "{route}");
    let left_out: Vec<(String, String)> = route["excluded"]
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
    assert_eq!(left_out.len(), 2, "{left_out:?}");
    assert_eq!(left_out[0].0, "claude-code");
    assert!(
        left_out[0]
            .1
            .contains("logged out (no ~/.claude/.credentials.json"),
        "{left_out:?}"
    );
    assert_eq!(left_out[1].0, "goose");
    assert!(
        left_out[1].1.contains("login cannot be told"),
        "{left_out:?}"
    );
    let text = stdout(&repo.ok(&["fleet", "route", "Fix the bug", "--seed", "1"]));
    assert!(
        text.contains(
            "would route as bugfix (classifier: fix, bug) by the inventory default (no \
             [fleet]): candidates codex, gemini-cli; seed 1"
        ),
        "{text}"
    );
    assert!(
        text.contains("not claude-code: claude-code is likely logged out"),
        "{text}"
    );

    // A routed run says where its route came from, and records it.
    let prompt = "Add a file WRITE added.txt=1";
    let seed = repo.seed_for(prompt, "gemini-cli");
    let out = repo.ok(&[
        "run",
        prompt,
        "--auto",
        "-n",
        "inventory",
        "--seed",
        &seed,
        "--yes",
    ]);
    let err = stderr(&out);
    assert!(
        err.contains("by: routed as feature (classifier: add"),
        "{err}"
    );
    assert!(
        err.contains(&format!(
            ") by the inventory default (no [fleet]): candidates codex, gemini-cli; seed {seed}"
        )),
        "{err}"
    );
    assert!(err.contains("by:   not goose:"), "{err}");
    let show = repo.json(&["show", "inventory", "--json"]);
    assert_eq!(show["status"]["state"], "ready", "{show}");
    let log = stdout(&repo.ok(&["log", "inventory"]));
    assert!(
        log.contains("routed feature (classifier) by the inventory default to gemini-cli"),
        "{log}"
    );

    // [defaults] harness goes first; a [fleet] is the configuration's.
    repo.config("[defaults]\nharness = \"gemini-cli\"\n");
    let text = stdout(&repo.ok(&["fleet", "route", "Fix the bug", "--seed", "1"]));
    assert!(
        text.contains("candidates gemini-cli, codex; seed 1"),
        "{text}"
    );
    repo.config(TWO);
    let route = repo.json(&["fleet", "route", "Fix the bug", "--seed", "1", "--json"]);
    assert_eq!(route["entry"], "default", "{route}");
    assert!(route["excluded"].as_array().unwrap().is_empty(), "{route}");
}
