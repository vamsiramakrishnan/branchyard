//! Operator workflows against the fake ACP agent, with private configuration
//! and PATH. No real harness, model, cloud service or adopted lead session.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use serde_json::{json, Value};

struct Fixture {
    repo: branchyard_testkit::Repo,
    agent: String,
}

impl Fixture {
    fn new() -> Self {
        let mut repo = branchyard_testkit::repo!();
        let tools = repo.dir.join("tools");
        branchyard_testkit::system_tools(&tools).unwrap();
        let home = repo.dir.join("home");
        fs::create_dir_all(&home).unwrap();
        repo.set_env("PATH", tools.to_str().unwrap());
        repo.set_env("HOME", home.to_str().unwrap());
        repo.set_env(
            "BRANCHYARD_HOME",
            repo.dir.join("by-home").to_str().unwrap(),
        );
        let agent = branchyard_testkit::fake_agent!().display().to_string();
        Self { repo, agent }
    }

    fn reply(&self, name: &str, value: &str) -> String {
        let path = self.repo.dir.join(format!("{name}.json"));
        fs::write(&path, value).unwrap();
        format!("Research the alternatives\nREPLY_FILE {}", path.display())
    }

    fn run_at(&self, directory: &Path, args: &[&str]) -> Output {
        self.repo
            .command(self.repo.by_path())
            .current_dir(directory)
            .args(args)
            .output()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_at(&self.repo.root, args)
    }

    fn root_run(&self, prompt: &str, name: &str, extra: &[&str]) -> Output {
        let mut args = vec![
            "run",
            prompt,
            "--name",
            name,
            "--harness",
            "gemini-cli",
            "--command",
            &self.agent,
            "--no-preflight",
            "--yes",
            "--json",
            "--result-schema",
            SCHEMA,
        ];
        args.extend(extra);
        self.run(&args)
    }

    fn fan_at(&self, directory: &Path, prompt: &str, name: &str, extra: &[&str]) -> Output {
        let mut args = vec![
            "fan",
            prompt,
            "--name",
            name,
            "--harness",
            "gemini-cli,qwen-code",
            "--command",
            &self.agent,
            "--no-preflight",
            "--yes",
            "--json",
            "--result-schema",
            SCHEMA,
            "--result-deliverable",
        ];
        args.extend(extra);
        self.run_at(directory, &args)
    }

    fn rig_spec(&self, name: &str, purpose: &str, failing_check: bool) -> String {
        let path = self.repo.dir.join(format!("{name}.toml"));
        let check = if failing_check {
            "check = [\"sh\", \"-c\", \"exit 1\"]"
        } else {
            ""
        };
        fs::write(&path, format!(r#"
version = 1
name = "{name}"
root = "lead"
[seats.lead]
harness = "gemini-cli"
policy = {{ default = "deny" }}
result = {{ purpose = "{purpose}", required = true, schema = {{ type = "object", required = ["answer"], properties = {{ answer = {{ type = "string", minLength = 1 }} }}, additionalProperties = false }} }}
{check}
"#)).unwrap();
        path.display().to_string()
    }
}

const SCHEMA: &str = r#"{"type":"object","required":["answer"],"properties":{"answer":{"type":"string","minLength":1}},"additionalProperties":false}"#;

fn shown(output: &Output) -> String {
    format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn parsed(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("one JSON response expected: {error}\n{}", shown(output)))
}

#[test]
fn root_native_json_is_visible_without_git_changes_or_delegated_children() {
    let f = Fixture::new();
    let prompt = f.reply("valid", r#"{"answer":"evidence"}"#);
    let output = f.root_run(&prompt, "research", &["--result-deliverable"]);
    assert!(output.status.success(), "{}", shown(&output));
    let result = parsed(&output);
    assert_eq!(result["status"]["state"], "no_changes");
    assert!(result["candidate"].is_null());
    assert_eq!(result["result"], json!({"answer":"evidence"}));
    assert_eq!(result["result_complete"], true);
    assert!(result["result_event"].as_u64().is_some());
    assert!(result["result_error"].is_null());
    assert_eq!(result["turns"], 1);
    assert_eq!(result["children"], json!([]));
    assert_eq!(result["result_contract"]["purpose"], "deliverable");
}

#[test]
fn invalid_required_outputs_and_failed_declared_checks_exit_one_with_one_report() {
    let f = Fixture::new();
    let invalid = f.reply("invalid", r#"{"answer":42}"#);
    for (name, flags) in [
        ("deliverable", vec!["--result-deliverable"]),
        ("metadata", vec!["--result-required"]),
    ] {
        let output = f.root_run(&invalid, name, &flags);
        assert_eq!(output.status.code(), Some(1), "{}", shown(&output));
        let result = parsed(&output);
        assert_eq!(result["result_complete"], false);
        assert!(result["result"].is_null());
        assert!(result["result_error"].is_string());
        assert_eq!(result["turns"], 1);
        assert_eq!(result["children"], json!([]));
        assert!(
            result.get("error").is_none(),
            "don't append another JSON envelope"
        );
    }
    let valid = f.reply("checked", r#"{"answer":"valid structure"}"#);
    let output = f.root_run(
        &valid,
        "failed-check",
        &["--result-deliverable", "--check", "sh -c 'exit 1'"],
    );
    assert_eq!(output.status.code(), Some(1), "{}", shown(&output));
    let result = parsed(&output);
    assert_eq!(result["result"], json!({"answer":"valid structure"}));
    assert_eq!(result["result_check"]["state"], "failed");
    assert_eq!(result["result_complete"], false);
    assert!(result["candidate"].is_null());
    assert_eq!(result["turns"], 1);
}

#[test]
fn rig_json_enforces_required_results_without_changing_its_shape_or_native_turns() {
    let f = Fixture::new();
    for purpose in ["report", "deliverable"] {
        let spec = f.rig_spec(&format!("rig-{purpose}"), purpose, false);
        for (label, answer, expected) in [
            ("valid", r#"{"answer":"finding"}"#, 0),
            ("missing", "no structured result", 1),
            ("invalid", r#"{"answer":42}"#, 1),
        ] {
            let prompt = f.reply(&format!("{purpose}-{label}"), answer);
            let name = format!("{purpose}-{label}");
            let output = f.run(&[
                "rig",
                "run",
                &spec,
                &prompt,
                "--name",
                &name,
                "--command",
                &f.agent,
                "--json",
            ]);
            assert_eq!(output.status.code(), Some(expected), "{}", shown(&output));
            let value = parsed(&output);
            assert_eq!(value["rig"], format!("rig-{purpose}"));
            assert_eq!(value["descendants"], json!([]));
            assert!(value.get("error").is_none());
            let root = &value["root"];
            assert_eq!(root["status"]["state"], "no_changes");
            assert_eq!(root["turns"], 1);
            assert_eq!(root["children"], json!([]));
            assert!(root["candidate"].is_null());
            assert_eq!(root["result_complete"], expected == 0);
            if expected == 0 {
                assert_eq!(root["result"], json!({"answer":"finding"}));
                assert!(root["result_event"].as_u64().is_some());
                assert!(root["result_error"].is_null());
            } else {
                assert!(root["result"].is_null());
                assert!(root["result_event"].is_null());
                assert!(root["result_error"].is_string());
            }
        }
    }
}

#[test]
fn rig_declared_checks_gate_deliverables_in_both_output_formats() {
    let f = Fixture::new();
    let spec = f.rig_spec("checked-rig", "deliverable", true);
    let prompt = f.reply("checked-rig", r#"{"answer":"valid structure"}"#);
    for json in [true, false] {
        let name = if json { "checked-json" } else { "checked-text" };
        let mut args = vec![
            "rig",
            "run",
            &spec,
            &prompt,
            "--name",
            name,
            "--command",
            &f.agent,
        ];
        if json {
            args.push("--json");
        }
        let output = f.run(&args);
        assert_eq!(output.status.code(), Some(1), "{}", shown(&output));
        if json {
            let value = parsed(&output);
            assert_eq!(value["rig"], "checked-rig");
            assert_eq!(value["descendants"], json!([]));
            assert_eq!(value["root"]["result"], json!({"answer":"valid structure"}));
            assert_eq!(value["root"]["result_check"]["state"], "failed");
            assert_eq!(value["root"]["result_complete"], false);
            assert_eq!(value["root"]["turns"], 1);
            assert!(value.get("error").is_none());
        } else {
            assert!(String::from_utf8_lossy(&output.stdout).contains("configured check failed"));
        }
    }
}

#[test]
fn fan_reports_native_results_and_invalid_fans_fail_without_extra_turns() {
    let f = Fixture::new();
    let prompt = f.reply(
        "fan",
        "Native planning can precede the final answer.\n```json\n{\"answer\":\"finding\"}\n```",
    );
    let output = f.fan_at(&f.repo.root, &prompt, "answers", &["--judge"]);
    assert!(output.status.success(), "{}", shown(&output));
    let result = parsed(&output);
    assert_eq!(result["branches"].as_array().unwrap().len(), 2);
    for branch in result["branches"].as_array().unwrap() {
        assert_eq!(branch["result"], json!({"answer":"finding"}));
        assert_eq!(branch["result_complete"], true);
        assert!(branch["candidate"].is_null());
        assert_eq!(branch["turns"], 1);
        assert_eq!(branch["children"], json!([]));
    }
    assert!(result["judgement"]["pick"].is_string());
    let invalid = f.reply("badfan", "no structured answer");
    let output = f.fan_at(&f.repo.root, &invalid, "incomplete", &[]);
    assert_eq!(output.status.code(), Some(1), "{}", shown(&output));
    let result = parsed(&output);
    assert!(result["branches"]
        .as_array()
        .unwrap()
        .iter()
        .all(|branch| branch["result_complete"] == false && branch["turns"] == 1));
}

#[test]
fn knowledge_judging_returns_a_result_pick_and_refuses_git_pick() {
    let f = Fixture::new();
    let prompt = f.reply("judge", r#"{"answer":"finding"}"#);
    let fan = f.fan_at(&f.repo.root, &prompt, "choices", &[]);
    assert!(fan.status.success(), "{}", shown(&fan));
    let output = f.run(&["judge", "choices", "--deterministic", "--json"]);
    assert!(output.status.success(), "{}", shown(&output));
    let judged = parsed(&output);
    assert!(judged["pick"].is_string());
    assert!(judged["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .all(|candidate| candidate["attempt"]["result"].is_object()));
    let output = f.run(&["judge", "choices", "--deterministic", "--pick", "--json"]);
    assert_eq!(output.status.code(), Some(4), "{}", shown(&output));
    assert_eq!(parsed(&output)["error"]["kind"], "unsupported");
    let bad_verdict = f.repo.dir.join("bad-verdict.txt");
    fs::write(&bad_verdict, "not a judge verdict").unwrap();
    let rubric = format!("REPLY_FILE {}", bad_verdict.display());
    let output = f.run(&[
        "judge",
        "choices",
        "--harness",
        "gemini-cli",
        "--command",
        &f.agent,
        "--rubric",
        &rubric,
        "--require-judge",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(1), "{}", shown(&output));
    let judged = parsed(&output);
    assert_eq!(judged["by"]["by"], "required_failed");
    assert!(judged["pick"].is_null());
    assert!(judged["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .all(|candidate| candidate["eligible"] == false));
}

#[test]
fn no_files_fan_works_outside_git_and_task_show_preserves_current_answers() {
    let f = Fixture::new();
    let outside = f.repo.dir.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let sentinel = outside.join("notes.txt");
    fs::write(&sentinel, "user notes stay untouched\n").unwrap();
    let prompt = f.reply("private", r#"{"answer":"private finding"}"#);
    let output = f.fan_at(&outside, &prompt, "private", &["--no-files", "--judge"]);
    assert!(output.status.success(), "{}", shown(&output));
    let fan = parsed(&output);
    let task = fan["task"].as_str().unwrap();
    let output = f.run_at(&outside, &["task", "show", task, "--json"]);
    assert!(output.status.success(), "{}", shown(&output));
    let shown_task = parsed(&output);
    assert_eq!(shown_task["files"]["kind"], "no_files");
    assert_eq!(shown_task["attempts"].as_array().unwrap().len(), 2);
    for attempt in shown_task["attempts"].as_array().unwrap() {
        assert_eq!(attempt["result"], json!({"answer":"private finding"}));
        assert_eq!(attempt["result_contract"]["purpose"], "deliverable");
        assert!(attempt["result_event"].as_u64().is_some());
        assert!(attempt.get("result_error").is_none());
        assert_eq!(attempt["turns"], 1);
    }
    let attempt = fan["branches"][0]["name"].as_str().unwrap();
    let output = f.run_at(
        &outside,
        &["task", "open", task, "--attempt", attempt, "--print"],
    );
    assert!(output.status.success(), "{}", shown(&output));
    let worktree = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    let output = f.run_at(
        &worktree,
        &["judge", "private", "--deterministic", "--json"],
    );
    assert!(output.status.success(), "{}", shown(&output));
    assert!(parsed(&output)["pick"].is_string());
    let recipient = fan["branches"][1]["name"].as_str().unwrap();
    let output = f.run_at(&worktree, &["compare", attempt, recipient, "--json"]);
    assert!(output.status.success(), "{}", shown(&output));
    let output = f.run_at(
        &worktree,
        &["handoff", attempt, "--to", recipient, "--json"],
    );
    assert!(output.status.success(), "{}", shown(&output));
    let handoff = parsed(&output);
    assert_eq!(
        handoff["handoff"]["result"],
        json!({"answer":"private finding"})
    );
    assert_eq!(handoff["next_turn"], true);
    let invalid = f.reply("revised", "this revised answer has no structured result");
    let output = f.run_at(&worktree, &["send", attempt, &invalid, "--yes", "--json"]);
    assert!(output.status.success(), "{}", shown(&output));
    let output = f.run_at(&outside, &["task", "show", task, "--json"]);
    assert!(output.status.success(), "{}", shown(&output));
    let latest = parsed(&output);
    let revised = latest["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|view| view["name"] == attempt)
        .unwrap();
    assert!(revised.get("result").is_none());
    assert!(revised.get("result_event").is_none());
    assert!(revised["result_error"].is_string());
    assert_eq!(revised["turns"], 2);
    assert_eq!(
        latest["attempts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|view| view["name"] == recipient)
            .unwrap()["turns"],
        1
    );
    assert_eq!(
        fs::read_to_string(&sentinel).unwrap(),
        "user notes stay untouched\n"
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);

    // A normal repository cannot redirect commands into this task by
    // copying its metadata: the shared git directory must be the same.
    let task_dir = PathBuf::from(shown_task["repository"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    let owner_gitfile = task_dir.join("work/.git");
    let original_gitfile = fs::read(&owner_gitfile).unwrap();
    fs::write(
        &owner_gitfile,
        format!("gitdir: {}\n", f.repo.root.join(".git").display()),
    )
    .unwrap();
    let output = f.run_at(&worktree, &["ls", "--json"]);
    fs::write(&owner_gitfile, original_gitfile).unwrap();
    assert_eq!(output.status.code(), Some(4), "{}", shown(&output));
    assert_eq!(parsed(&output)["error"]["kind"], "denied");
    f.repo.git(&["config", "branchyard.task", task]);
    let output = f.run(&["ls", "--json"]);
    assert_eq!(output.status.code(), Some(4), "{}", shown(&output));
    assert_eq!(parsed(&output)["error"]["kind"], "denied");
    f.repo
        .git(&["config", "branchyard.home", task_dir.to_str().unwrap()]);
    let output = f.run(&["ls", "--json"]);
    assert_eq!(output.status.code(), Some(4), "{}", shown(&output));
    assert_eq!(parsed(&output)["error"]["kind"], "denied");
    assert!(!f.repo.root.join(".branchyard").exists());
}

#[test]
fn no_files_refuses_remote_before_creating_a_private_task() {
    let f = Fixture::new();
    let output = f.run(&[
        "--remote",
        "https://unused.example",
        "fan",
        "research",
        "--harness",
        "gemini-cli",
        "--no-files",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(2), "{}", shown(&output));
    assert_eq!(parsed(&output)["error"]["kind"], "usage");
    assert!(!f.repo.dir.join("by-home/tasks").exists());
}

#[test]
fn private_fan_resolves_a_schema_from_stdin_once() {
    let f = Fixture::new();
    let outside = f.repo.dir.join("outside");
    fs::create_dir_all(&outside).unwrap();
    let prompt = f.reply("stdin-schema", r#"{"answer":"read once"}"#);
    let mut child = f
        .repo
        .command(f.repo.by_path())
        .current_dir(&outside)
        .args([
            "fan",
            &prompt,
            "--no-files",
            "--name",
            "stdin",
            "--harness",
            "gemini-cli,qwen-code",
            "--command",
            &f.agent,
            "--no-preflight",
            "--yes",
            "--json",
            "--result-schema-file",
            "-",
            "--result-deliverable",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(SCHEMA.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", shown(&output));
    let value = parsed(&output);
    assert!(value["task"].is_string());
    assert!(value["branches"]
        .as_array()
        .unwrap()
        .iter()
        .all(|branch| branch["result"] == json!({"answer":"read once"})));
}
