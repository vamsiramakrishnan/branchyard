//! What a failed `by` says and prints: the object that failed, why when
//! that is not obvious, and the command to run next, as text on standard
//! error and, with `--json`, as an envelope on standard output. The built
//! `by`, a temporary repository and the fake ACP agent; hermetic, with no
//! real harness and no network. Requires `git` and `sh`.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::process::{Command, Output};

use branchyard_testkit::{fake_agent, wait};
use serde_json::Value;

/// The kit's repository, plus what this file adds.
struct Repo(branchyard_testkit::Repo);

impl std::ops::Deref for Repo {
    type Target = branchyard_testkit::Repo;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Repo {
    fn new() -> Repo {
        Repo(branchyard_testkit::repo!())
    }

    /// A finished branch `name`, written by the fake agent, with `extra`
    /// options of `by run`.
    fn branch(&self, name: &str, extra: &[&str]) {
        let mut args = vec!["run", "WRITE f.txt=1", "--name", name];
        args.extend(extra);
        let out = self.by_agent(&args);
        assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    }

    /// `by <args>` in the scratch directory above the repository, which is
    /// no repository.
    fn outside(&self, args: &[&str]) -> Output {
        branchyard_testkit::hermetic(&mut Command::new(env!("CARGO_BIN_EXE_by")))
            .args(args)
            .current_dir(&self.dir)
            .env("BRANCHYARD_USER_CONFIG", self.dir.join("user/config.toml"))
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The envelope a failed `--json` command printed on standard output.
fn envelope(out: &Output) -> Value {
    let text = stdout(out);
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}):\n{text}\nstderr:\n{}",
            stderr(out)
        )
    });
    assert!(value["error"].is_object(), "{text}");
    value["error"].clone()
}

#[test]
fn outside_a_repository_the_message_names_the_directory_and_the_way_out() {
    let repo = Repo::new();
    let here = fs::canonicalize(&repo.dir).unwrap();
    let out = repo.outside(&["ls"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        stderr(&out),
        format!(
            "by: {} is not inside a git repository\n  next: cd into your project, or run \
             `git init` here\n",
            here.display()
        )
    );
    assert_eq!(stdout(&out), "");

    let json = repo.outside(&["ls", "--json"]);
    assert_eq!(json.status.code(), Some(1));
    let error = envelope(&json);
    assert_eq!(error["kind"], "not_a_repository");
    assert_eq!(
        error["message"],
        format!("{} is not inside a git repository", here.display())
    );
    assert_eq!(
        error["next"][0],
        "cd into your project, or run `git init` here"
    );
    assert_eq!(stderr(&json), "");
}

#[test]
fn an_unknown_branch_gets_a_did_you_mean_and_by_ls() {
    let repo = Repo::new();
    repo.branch("broken", &[]);
    repo.branch("hello", &[]);

    let typo = repo.by(&["show", "brokn"]);
    assert_eq!(typo.status.code(), Some(1));
    assert_eq!(
        stderr(&typo),
        "by: no branch named brokn; did you mean broken?\n  next: by ls\n"
    );
    let far = repo.by(&["diff", "zzzzzzzz"]);
    assert_eq!(
        stderr(&far),
        "by: no branch named zzzzzzzz\n  next: by ls\n"
    );

    let json = repo.by(&["show", "brokn", "--json"]);
    assert_eq!(json.status.code(), Some(1));
    let error = envelope(&json);
    assert_eq!(error["kind"], "unknown_branch");
    assert_eq!(
        error["message"],
        "no branch named brokn; did you mean broken?"
    );
    assert_eq!(error["next"][0], "by ls");
}

#[test]
fn a_running_branch_says_to_wait_for_it_or_cancel_it() {
    let repo = Repo::new();
    let agent = fake_agent!().display().to_string();
    let running = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .args(["run", "HANG", "--name", "stuck", "--harness", "gemini-cli"])
        .args(["--command", &agent])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let runner = std::thread::spawn(move || running.wait_with_output().unwrap());
    wait::until("the turn to start", || {
        stdout(&repo.by(&["log", "stuck"])).contains("prompt: HANG")
    });

    let refused = repo.by_agent(&["send", "stuck", "WHOAMI"]);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        stderr(&refused).ends_with(
            "by: stuck is still running its turn\n  next: by wait stuck (blocks until it settles), \
             or by cancel stuck (stops it)\n"
        ),
        "{}",
        stderr(&refused)
    );
    let json = repo.by_agent(&["send", "stuck", "WHOAMI", "--json"]);
    let error = envelope(&json);
    assert_eq!(error["kind"], "running");
    assert_eq!(error["message"], "stuck is still running its turn");
    assert_eq!(error["next"].as_array().unwrap().len(), 2);

    repo.ok(&["cancel", "stuck"]);
    let ran = runner.join().unwrap();
    assert!(ran.status.success(), "{}", stderr(&ran));
}

#[test]
fn a_failed_check_names_the_check_the_exit_the_output_and_what_to_run() {
    let repo = Repo::new();
    repo.branch("broken", &["--check", "sh -c \"echo nope >&2; exit 3\""]);

    let merge = repo.by(&["merge", "broken"]);
    assert_eq!(merge.status.code(), Some(1));
    assert_eq!(stdout(&merge), "");
    assert_eq!(
        stderr(&merge),
        "by: the check for broken failed (exit 3); nothing was merged.\n  check: sh -c 'echo \
         nope >&2; exit 3'\n  output: nope\n  next: by check broken (runs the check without \
         merging)\n"
    );

    let json = repo.by(&["merge", "broken", "--json"]);
    assert_eq!(json.status.code(), Some(1));
    let error = envelope(&json);
    assert_eq!(error["kind"], "check_failed");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with("the check for broken failed (exit 3); nothing was merged.\ncheck: sh"),
        "{error}"
    );
    assert_eq!(error["detail"]["status"], "exit 3");
    assert_eq!(
        error["detail"]["command"],
        serde_json::json!(["sh", "-c", "echo nope >&2; exit 3"])
    );
    assert_eq!(
        error["next"][0],
        "by check broken (runs the check without merging)"
    );

    // Nothing was merged: the branch is still ready.
    assert_eq!(
        repo.json(&["show", "broken", "--json"])["status"]["state"],
        "ready"
    );
}

#[test]
fn show_and_the_summary_say_a_check_runs_at_merge_and_offer_to_run_it() {
    let repo = Repo::new();
    let out = repo.by_agent(&[
        "run",
        "WRITE f.txt=1",
        "--name",
        "checked",
        "--check",
        "true",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let summary = stdout(&out);
    assert!(
        summary.contains("check")
            && summary.contains("true (runs at merge; `by check checked` runs it now)"),
        "{summary}"
    );
    // In the next commands, the check comes between the diff and the merge.
    let next = &summary[summary.find("\nnext").unwrap()..];
    let diff = next.find("by diff checked").unwrap();
    let check = next.find("by check checked").unwrap();
    let merge = next.find("by merge checked").unwrap();
    assert!(diff < check && check < merge, "{summary}");

    let shown = stdout(&repo.ok(&["show", "checked"]));
    assert!(
        shown.contains("check")
            && shown.contains("true (its own); runs at merge, or now with `by check checked`"),
        "{shown}"
    );

    // A branch with no check has neither the line nor the command.
    let plain = repo.by_agent(&["run", "WRITE g.txt=1", "--name", "plain"]);
    assert!(
        !stdout(&plain).contains("by check plain"),
        "{}",
        stdout(&plain)
    );
}

#[test]
fn a_merge_says_which_command_frees_the_name() {
    let repo = Repo::new();
    repo.branch("one", &[]);
    repo.branch("two", &[]);
    let text = stdout(&repo.ok(&["merge", "one", "--into", "main"]));
    assert!(
        text.starts_with("merged one into main (")
            && text.ends_with("); `by rm one` frees the name\n"),
        "{text}"
    );
    // With --rm the name is already free.
    let removed = stdout(&repo.ok(&["merge", "two", "--into", "main", "--rm"]));
    assert!(!removed.contains("frees the name"), "{removed}");
    assert!(removed.ends_with("removed two\n"), "{removed}");
}

#[test]
fn a_missing_harness_says_how_to_install_or_pick_one() {
    let repo = Repo::new();
    // A PATH with git and sh and nothing else: no `gemini`.
    let bin = repo.dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    for tool in ["git", "sh"] {
        let found = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|dir| dir.join(tool))
            .find(|path| path.is_file())
            .unwrap();
        std::os::unix::fs::symlink(found, bin.join(tool)).unwrap();
    }
    let run = |extra: &[&str]| {
        repo.command(env!("CARGO_BIN_EXE_by"))
            .args(["run", "x", "--harness", "gemini-cli"])
            .args(extra)
            .env("PATH", &bin)
            .output()
            .unwrap()
    };
    let out = run(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).ends_with(
            "by: gemini-cli is not installed: gemini was not found on PATH\n  next: by harnesses \
             install gemini-cli, or pick another with by harnesses\n"
        ),
        "{}",
        stderr(&out)
    );
    let json = run(&["--json"]);
    let error = envelope(&json);
    assert_eq!(error["kind"], "harness_unavailable");
    assert_eq!(
        error["message"],
        "gemini-cli is not installed: gemini was not found on PATH"
    );
    assert_eq!(error["next"][0], "by harnesses install gemini-cli");
}

#[test]
fn a_missing_argument_is_a_usage_sentence_not_a_denial() {
    let repo = Repo::new();
    let inspect = repo.by(&["inspect"]);
    assert_eq!(inspect.status.code(), Some(1));
    assert_eq!(
        stderr(&inspect),
        "by: inspect needs a branch outside a harness: `by inspect <branch>` (`by ls` lists \
         them)\n"
    );
    let ask = repo.by(&["ask", "why"]);
    assert_eq!(
        stderr(&ask),
        "by: ask needs --as <BRANCH> outside a harness, the branch that is acting: `by ask --as \
         <branch> ...` (`by ls` lists them)\n"
    );
    let wait = repo.by(&["wait"]);
    assert_eq!(
        stderr(&wait),
        "by: wait needs the branches to wait for outside a harness: `by wait <branch>...` (`by \
         ls` lists them)\n"
    );
    assert!(!stderr(&inspect).contains("denied"));
    let json = repo.by(&["inspect", "--json"]);
    assert_eq!(envelope(&json)["kind"], "usage");
}

#[test]
fn a_timeout_that_is_not_a_number_says_what_it_needs() {
    let repo = Repo::new();
    let out = repo.by(&["wait", "x", "--timeout", "soon"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains(
            "invalid value 'soon' for '--timeout <SECONDS>': needs a number of seconds, such as \
             30 or 2.5"
        ),
        "{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("float"), "{}", stderr(&out));
}

#[test]
fn a_config_value_of_the_wrong_type_is_a_sentence() {
    let repo = Repo::new();
    fs::write(
        repo.root.join("branchyard.toml"),
        "[defaults]\nbudget_usd = \"five\"\n",
    )
    .unwrap();
    let out = repo.by(&["ls"]);
    assert_eq!(out.status.code(), Some(1));
    let text = stderr(&out);
    assert!(
        text.contains("line 2: defaults.budget_usd needs a number, not \"five\""),
        "{text}"
    );
    assert!(!text.contains("f64"), "{text}");
    assert!(
        text.contains("(fix it, or check it with `by config validate`)"),
        "{text}"
    );

    let json = repo.by(&["ls", "--json"]);
    assert_eq!(json.status.code(), Some(1));
    let error = envelope(&json);
    assert_eq!(error["kind"], "config");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("defaults.budget_usd needs a number"),
        "{error}"
    );
}

#[test]
fn a_refused_command_line_is_json_too_with_exit_2() {
    let repo = Repo::new();
    let out = repo.by(&["run", "--json", "--bogus"]);
    assert_eq!(out.status.code(), Some(2));
    let error = envelope(&out);
    assert_eq!(error["kind"], "usage");
    assert!(
        error["message"].as_str().unwrap().contains("--bogus"),
        "{error}"
    );
    assert_eq!(stderr(&out), "");
    // Help is still help.
    let help = repo.by(&["run", "--help", "--json"]);
    assert_eq!(help.status.code(), Some(0));
    assert!(stdout(&help).contains("Usage: by run"), "{}", stdout(&help));
}

#[test]
fn a_server_that_does_not_answer_is_remote_unreachable() {
    let repo = Repo::new();
    let token = repo.dir.join("token");
    fs::write(&token, "secret\n").unwrap();
    // A Unix socket nobody listens on: refused at once, no port involved.
    let socket = format!("unix:{}", repo.dir.join("nobody.sock").display());
    let out = repo.by(&[
        "--remote",
        &socket,
        "--token-file",
        token.to_str().unwrap(),
        "ls",
        "--json",
    ]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let error = envelope(&out);
    assert_eq!(error["kind"], "remote_unreachable");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with("cannot reach "),
        "{error}"
    );
}

/// Every command that takes a branch reports its failures as the envelope
/// when `--json` is on the line, whichever path found the branch missing.
#[test]
fn every_branch_command_reports_an_unknown_branch_as_json() {
    let repo = Repo::new();
    let missing = "nope";
    let lines: &[&[&str]] = &[
        &["show", missing, "--json"],
        &["log", missing, "--json"],
        &["send", missing, "hi", "--json"],
        &["steer", missing, "hi", "--json"],
        &["cancel", missing, "--json"],
        &["merge", missing, "--json"],
        &["rm", missing, "--json"],
        &["rewind", missing, "--to", "0", "--yes", "--json"],
        &["compare", missing, "--json"],
        &["undo", missing, "--json"],
        &["pr", missing, "--json"],
        &["review", missing, "--json"],
        &["fork", missing, "hi", "--json"],
        &["check", missing, "--json"],
        &["inspect", missing, "--json"],
        &["wait", missing, "--json"],
    ];
    // A command with no JSON output refuses `--json` as a usage error, in
    // JSON, with clap's exit status.
    let out = repo.by(&["diff", missing, "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(envelope(&out)["kind"], "usage");
    // A task is not a branch: its refusal is a sentence about the argument.
    let task = repo.by(&["task", "show", missing, "--json"]);
    assert_eq!(task.status.code(), Some(1), "{}", stderr(&task));
    let error = envelope(&task);
    assert_eq!(error["kind"], "usage");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with("no task nope"),
        "{error}"
    );
    for line in lines {
        let out = repo.by(line);
        assert_eq!(out.status.code(), Some(1), "{line:?}: {}", stderr(&out));
        let error = envelope(&out);
        let kind = error["kind"].as_str().unwrap_or_default();
        assert_eq!(kind, "unknown_branch", "{line:?}: {error}");
        assert_eq!(stderr(&out), "", "{line:?}");
    }
}

/// `fan` takes `--json` and prints its branches; `rm` and `merge` print
/// what they did; `run --json` waits for the turn and prints the branch as
/// `show --json` does, with the activity on standard error.
#[test]
fn run_fan_merge_and_rm_print_json() {
    let repo = Repo::new();
    let agent = fake_agent!().display().to_string();

    let run = repo.by_agent(&["run", "WRITE a.txt=1", "--name", "first", "--json"]);
    assert!(run.status.success(), "{}", stderr(&run));
    let shown: Value = serde_json::from_slice(&run.stdout)
        .unwrap_or_else(|e| panic!("run --json printed {e}:\n{}", stdout(&run)));
    assert_eq!(shown["name"], "first");
    assert_eq!(shown["status"]["state"], "ready");
    assert!(shown.get("merge_readiness").is_some(), "{shown}");
    assert!(stderr(&run).contains("wrote a.txt"), "{}", stderr(&run));
    assert_eq!(shown, repo.json(&["show", "first", "--json"]));

    let fan = repo.by(&[
        "fan",
        "WRITE b.txt=1",
        "--harness",
        "gemini-cli,qwen-code",
        "--command",
        &agent,
        "--json",
    ]);
    assert!(fan.status.success(), "{}\n{}", stdout(&fan), stderr(&fan));
    let fanned: Value = serde_json::from_slice(&fan.stdout).unwrap();
    let branches = fanned["branches"].as_array().unwrap();
    assert_eq!(branches.len(), 2, "{fanned}");
    assert!(branches.iter().all(|b| b["status"]["state"] == "ready"));
    assert!(fanned.get("judgement").is_none());

    let merge = repo.json(&["merge", "first", "--into", "main", "--json"]);
    assert_eq!(merge["branch"], "first");
    assert_eq!(merge["target"], "main");
    assert_eq!(merge["commit"].as_str().unwrap().len(), 40, "{merge}");
    assert_eq!(merge["check"], Value::Null);
    assert_eq!(merge["removed"], false);

    let gone = repo.json(&["rm", "first", "--json"]);
    assert_eq!(gone, serde_json::json!({"removed": "first"}));

    // A merge with a check says it passed, and --rm removes the branch.
    let checked = repo.by_agent(&[
        "run",
        "WRITE c.txt=1",
        "--name",
        "second",
        "--check",
        "true",
    ]);
    assert!(checked.status.success());
    let merged = repo.json(&["merge", "second", "--into", "main", "--rm", "--json"]);
    assert_eq!(merged["check"]["command"], serde_json::json!(["true"]));
    assert_eq!(merged["check"]["result"], "passed");
    assert_eq!(merged["removed"], true);
    assert!(!repo.root.join(".branchyard/worktrees/second").exists());
}

/// The local-mode note is about the repository, so it is said the first
/// time and not on every run.
#[test]
fn the_local_mode_note_is_printed_once_per_repository() {
    let repo = Repo::new();
    let first = repo.by_agent(&["run", "WRITE a.txt=1", "--name", "a"]);
    assert!(first.status.success());
    assert!(
        stderr(&first).contains("no isolation"),
        "{}",
        stderr(&first)
    );
    let second = repo.by_agent(&["run", "WRITE b.txt=1", "--name", "b"]);
    assert!(second.status.success());
    assert!(
        !stderr(&second).contains("no isolation"),
        "{}",
        stderr(&second)
    );
    let send = repo.by_agent(&["send", "a", "WHOAMI"]);
    assert!(!stderr(&send).contains("no isolation"), "{}", stderr(&send));
}

/// `by mcp --list-tools` prints the tools and their input schemas without a
/// repository, a branch or a token.
#[test]
fn mcp_lists_its_tools_without_a_branch_or_a_token() {
    let repo = Repo::new();
    let out = repo.outside(&["mcp", "--list-tools"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let listing: Value = serde_json::from_slice(&out.stdout).unwrap();
    let tools = listing["tools"].as_array().unwrap();
    let spawn = tools.iter().find(|t| t["name"] == "spawn").unwrap();
    assert_eq!(spawn["inputSchema"]["type"], "object");
    assert!(
        spawn["inputSchema"]["properties"]["prompt"].is_object(),
        "{spawn}"
    );
    assert!(tools.len() >= 10, "{}", tools.len());
    assert!(repo.by(&["mcp", "--root", "/r"]).status.code() == Some(2));
}
