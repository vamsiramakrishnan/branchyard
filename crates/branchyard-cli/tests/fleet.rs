//! The fleet table through the built `by`, against temporary repositories
//! and the fake ACP agent: routing with a `[fleet]` and no `--harness`,
//! `--auto` failing over from a harness that exits, `--kind`, `by fan
//! --auto --judge` with a judge harness answering a canned verdict, `by
//! judge --pick`, `by fleet stats|route`, and the refusals. Hermetic: no
//! model is called. Requires `git` and `sh`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use serde_json::Value;

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
            "branchyard-cli-fleet-{}-{}",
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
        fs::write(repo.root.join(".gitignore"), "branchyard.toml\n").unwrap();
        repo.git(&["add", "."]);
        repo.git(&["commit", "-q", "-m", "initial"]);
        repo
    }

    /// Write `branchyard.toml`, with `AGENT` replaced by the fake agent.
    fn config(&self, text: &str) {
        let agent = fake_agent().display().to_string();
        fs::write(
            self.root.join("branchyard.toml"),
            text.replace("AGENT", &agent),
        )
        .unwrap();
    }

    fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            .env("BRANCHYARD_USER_CONFIG", self.dir.join("user/config.toml"))
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

    fn ok(&self, args: &[&str]) -> Output {
        let out = self.by(args);
        assert!(
            out.status.success(),
            "by {args:?}\nstdout:\n{}\nstderr:\n{}",
            stdout(&out),
            stderr(&out)
        );
        out
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_slice(&self.ok(args).stdout).unwrap()
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
    let agent = fake_agent().display().to_string();
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
    // --auto without a [fleet].
    let out = repo.by(&["run", "x", "--auto", "--yes"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("has none; add [fleet.default]"),
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
