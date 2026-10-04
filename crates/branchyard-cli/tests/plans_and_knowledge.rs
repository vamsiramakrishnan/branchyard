//! Plan approval, goals and repository knowledge through the built `by`,
//! against temporary repositories and the fake ACP agent: `by run --plan`,
//! `by plan show|approve|reject`, `--goal` with a judge harness answering a
//! sequence of canned verdicts, `[fleet.<kind>] plan`, and `by knowledge`
//! end to end, including `review` reading answers from stdin. Hermetic: no
//! model is called. Requires `git` and `sh`.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use branchyard_testkit::fake_agent;
use serde_json::Value;

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Repo {
    dir: PathBuf,
    root: PathBuf,
}

impl Repo {
    fn new() -> Repo {
        let dir = std::env::temp_dir().join(format!(
            "branchyard-cli-plans-{}-{}",
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

    /// Write `branchyard.toml`, with `AGENT` replaced by the fake agent and
    /// `DIR` by the test's directory.
    fn config(&self, text: &str) {
        let agent = fake_agent!().display().to_string();
        fs::write(
            self.root.join("branchyard.toml"),
            text.replace("AGENT", &agent)
                .replace("DIR", &self.dir.display().to_string()),
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
            .env("USER", "ana")
            .env("BRANCHYARD_USER_CONFIG", self.dir.join("user/config.toml"))
            .env("PAGER", "cat");
        for var in [
            "BRANCHYARD_DELEGATION",
            "BRANCHYARD_BRANCH",
            "BRANCHYARD_ROOT",
            "BRANCHYARD_BY",
            "BRANCHYARD_REMOTE",
            "VISUAL",
            "EDITOR",
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

    fn by_with(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = self
            .command(env!("CARGO_BIN_EXE_by"))
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
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn by(&self, args: &[&str]) -> Output {
        self.by_with(args, "")
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

    fn agent_args(&self) -> Vec<String> {
        vec![
            "--harness".into(),
            "gemini-cli".into(),
            "--command".into(),
            fake_agent!().display().to_string(),
        ]
    }

    fn run(&self, prompt: &str, name: &str, extra: &[&str]) -> Output {
        let mut args: Vec<String> = vec!["run".into(), prompt.into(), "-n".into(), name.into()];
        args.extend(self.agent_args());
        args.extend(extra.iter().map(|a| (*a).to_owned()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.by(&args)
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
fn a_planned_run_waits_and_is_approved_with_an_edited_plan() {
    let repo = Repo::new();
    let out = repo.run(
        "Change it PERMISSION WRITE marker.txt=x",
        "planned",
        &["--plan", "--yes"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("awaiting plan approval"), "{text}");
    assert!(text.contains("by plan approve planned"), "{text}");
    // Read-only, even with --yes: the write was denied.
    let log = repo.json(&["log", "planned", "--json"]);
    let denied = log
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["activity"] == "decision" && e["allowed"] == false);
    assert!(denied, "{log}");
    assert!(!repo
        .root
        .join(".branchyard/worktrees/planned/marker.txt")
        .exists());
    let plan = repo.json(&["plan", "show", "planned", "--json"]);
    assert_eq!(plan["phase"], "awaiting", "{plan}");
    assert_eq!(plan["plan"]["markdown"], "denied");
    let shown = stdout(&repo.ok(&["plan", "show", "planned"]));
    assert!(shown.contains("awaiting approval"), "{shown}");
    let show = stdout(&repo.ok(&["show", "planned"]));
    assert!(show.contains("round 1, awaiting approval"), "{show}");
    // A plain send is refused while the plan waits.
    let refused = repo.by(&["send", "planned", "WRITE x.txt=1", "--yes"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("plan awaits approval"));

    // --edit opens the editor on the plan; what it saves is approved.
    let editor = "sh -c 'printf \"WRITE edited.txt=yes\" > \"$0\"'";
    let approved = repo.json(&[
        "plan", "approve", "planned", "--edit", "--editor", editor, "--json",
    ]);
    assert_eq!(approved["status"]["state"], "ready", "{approved}");
    assert!(repo
        .root
        .join(".branchyard/worktrees/planned/edited.txt")
        .is_file());
    let plan = repo.json(&["plan", "show", "planned", "--json"]);
    assert_eq!(plan["phase"], "approved");
    let log = stdout(&repo.ok(&["log", "planned"]));
    assert!(
        log.contains("plan round 1 approved with edits by ana"),
        "{log}"
    );
    // Nothing is left to approve.
    let again = repo.by(&["plan", "approve", "planned", "--json"]);
    assert!(!again.status.success());
    let error: Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(error["error"]["kind"], "no_plan");
}

#[test]
fn a_plan_is_rejected_to_replan_then_to_end_and_a_fleet_entry_plans_first() {
    let repo = Repo::new();
    repo.config(
        r#"
[fleet.docs]
candidates = [{ harness = "gemini-cli", command = "AGENT" }]
plan = true
"#,
    );
    // A docs task with a named harness is not routed, and still plans
    // first, as its [fleet.docs] entry says.
    let out = repo.run("Document the parser", "docs", &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let show = repo.json(&["show", "docs", "--json"]);
    assert_eq!(show["status"]["state"], "awaiting_plan_approval", "{show}");
    assert_eq!(show["plan"]["phase"], "awaiting");

    let replanned = repo.json(&[
        "plan", "reject", "docs", "--reason", "shorter", "--replan", "--json",
    ]);
    assert_eq!(replanned["status"]["state"], "awaiting_plan_approval");
    let plan = repo.json(&["plan", "show", "docs", "--json"]);
    assert_eq!(plan["round"], 2, "{plan}");
    assert!(plan["plan"]["markdown"]
        .as_str()
        .unwrap()
        .contains("shorter"));

    let ended = repo.json(&["plan", "reject", "docs", "--reason", "not now", "--json"]);
    assert_eq!(ended["status"]["state"], "failed", "{ended}");
    assert!(ended["status"]["reason"]
        .as_str()
        .unwrap()
        .contains("rejected by ana: not now"));
}

#[test]
fn a_goal_judge_finds_it_unmet_then_met() {
    let repo = Repo::new();
    let verdicts = repo.dir.join("verdicts");
    fs::create_dir_all(&verdicts).unwrap();
    fs::write(
        verdicts.join("1.json"),
        r#"{"met": false, "evidence": ["one.txt exists"], "missing": ["two.txt: WRITE two.txt=2"]}"#,
    )
    .unwrap();
    fs::write(
        verdicts.join("2.json"),
        r#"{"met": true, "evidence": ["one.txt and two.txt exist"], "missing": []}"#,
    )
    .unwrap();
    repo.config(
        r#"
[fleet.default]
candidates = [{ harness = "gemini-cli", command = "AGENT" }]
goal_judge = { harness = "gemini-cli", command = "AGENT", rubric = "REPLY_SEQUENCE DIR/verdicts" }
"#,
    );
    let out = repo.run(
        "Add files WRITE one.txt=1",
        "goal",
        &["--goal", "one.txt and two.txt exist", "--check", "true"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let show = repo.json(&["show", "goal", "--json"]);
    assert_eq!(show["status"]["state"], "ready", "{show}");
    assert_eq!(show["goal"]["met"], true, "{show}");
    assert_eq!(show["goal"]["used"], 1);
    assert_eq!(show["goal"]["evidence"][0], "one.txt and two.txt exist");
    assert!(repo
        .root
        .join(".branchyard/worktrees/goal/two.txt")
        .is_file());
    let text = stdout(&repo.ok(&["show", "goal"]));
    assert!(
        text.contains("one.txt and two.txt exist — met: one.txt and two.txt exist"),
        "{text}"
    );
    let log = stdout(&repo.ok(&["log", "goal"]));
    assert!(
        log.contains("goal not met (round 0, harness gemini-cli)"),
        "{log}"
    );
    assert!(
        log.contains("goal met (round 1, harness gemini-cli)"),
        "{log}"
    );
    assert!(fs::read_dir(&verdicts).unwrap().next().is_none());
}

#[test]
fn an_unmet_goal_without_rounds_fails_the_run() {
    let repo = Repo::new();
    let out = repo.run(
        "Add WRITE x.txt=1",
        "unmet",
        &[
            "--goal",
            "the check passes",
            "--goal-rounds",
            "0",
            "--check",
            "false",
        ],
    );
    assert!(!out.status.success());
    let show = repo.json(&["show", "unmet", "--json"]);
    assert_eq!(show["goal"]["met"], false, "{show}");
    assert!(show["status"]["reason"]
        .as_str()
        .unwrap()
        .contains("goal was not met"));
}

#[test]
fn knowledge_is_added_reviewed_used_and_exported() {
    let repo = Repo::new();
    // A branch whose corrections become proposals when it merges.
    let out = repo.run("Add notes WRITE notes.txt=1", "notes", &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    repo.ok(&[
        "send",
        "notes",
        "Keep every note under eighty columns WRITE notes.txt=2",
    ]);
    repo.ok(&[
        "send",
        "notes",
        "Mention the ticket in each note please WRITE notes.txt=3",
    ]);
    repo.ok(&["merge", "notes"]);
    let proposed = repo.json(&["knowledge", "list", "--status", "proposed", "--json"]);
    assert_eq!(proposed.as_array().unwrap().len(), 2, "{proposed}");
    let first = proposed[0]["id"].as_u64().unwrap();
    let second = proposed[1]["id"].as_u64().unwrap();

    // Review: adopt the first, reject the second with a reason.
    let out = repo.by_with(&["knowledge", "review", "--json"], "a\nr\ntoo specific\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let review: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(review["decisions"][0]["decision"], "adopted", "{review}");
    assert_eq!(review["decisions"][1]["decision"], "rejected");
    let entry = repo.json(&["knowledge", "show", &second.to_string(), "--json"]);
    assert_eq!(entry["status"], "rejected");
    assert_eq!(entry["note"], "too specific");

    // A person's own entry, scoped to a path, adopted at once.
    let added = repo.json(&[
        "knowledge",
        "add",
        "Notes are plain text.",
        "--path",
        "*.txt",
        "--json",
    ]);
    assert_eq!(added["status"], "adopted");
    assert_eq!(added["adopted_by"], "ana");
    let mine = added["id"].as_u64().unwrap();
    // Editing a scope, then the text with --text.
    let edited = repo.json(&[
        "knowledge",
        "edit",
        &mine.to_string(),
        "--kind",
        "docs",
        "--json",
    ]);
    assert_eq!(edited["scope"]["kind"], "docs");
    assert_eq!(edited["scope"]["path"], "*.txt");

    // The adopted entries reach a matching branch, and its log says so.
    let out = repo.run(
        "Update a.txt SHOW_INSTRUCTIONS",
        "uses",
        &["--kind", "docs"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let log = stdout(&repo.ok(&["log", "uses"]));
    assert!(
        log.contains(&format!("knowledge #{mine}, #{first}")),
        "{log}"
    );
    let events = repo.json(&["log", "uses", "--json"]);
    let reply: String = events
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["event"]["text"].as_str())
        .collect();
    assert!(
        reply.contains("Keep every note under eighty columns"),
        "{reply}"
    );
    assert!(!reply.contains("Mention the ticket"), "{reply}");

    // Export, then remove.
    let exported = repo.root.join("AGENTS.md");
    repo.ok(&["knowledge", "export", "--out", exported.to_str().unwrap()]);
    let markdown = fs::read_to_string(&exported).unwrap();
    assert!(markdown.contains(&format!("(k{first})")), "{markdown}");
    assert!(markdown.contains("## Docs tasks on files matching `*.txt`"));
    repo.ok(&["knowledge", "rm", &mine.to_string()]);
    let missing = repo.by(&["knowledge", "show", &mine.to_string(), "--json"]);
    assert!(!missing.status.success());
    let error: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(error["error"]["kind"], "unknown_knowledge");

    // Distill on demand: nothing new, the rejection sticks.
    let distilled = repo.json(&["knowledge", "distill", "notes", "--json"]);
    assert_eq!(distilled["proposed"].as_array().unwrap().len(), 0);
    assert_eq!(distilled["duplicates"], 2);
}

#[test]
fn knowledge_settings_come_from_the_configuration() {
    let repo = Repo::new();
    repo.config(
        r#"
[knowledge]
provision = false
distill_on = []
"#,
    );
    repo.ok(&["knowledge", "add", "Never given while provision is off."]);
    let out = repo.run("Update a.txt SHOW_INSTRUCTIONS", "off", &[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let log = stdout(&repo.ok(&["log", "off"]));
    assert!(!log.contains("knowledge #"), "{log}");
    // A bad setting is refused, naming its key.
    repo.config("[knowledge]\ndistill_on = [\"weekly\"]\n");
    let refused = repo.by(&["knowledge", "list"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("knowledge.distill_on"),
        "{}",
        stderr(&refused)
    );
}
