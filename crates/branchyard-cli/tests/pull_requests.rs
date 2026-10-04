//! `by run --issue`, `by pr`, `by pr --watch`, `by show`'s merge readiness
//! and `by open`, end to end with the built `by`, the fake ACP agent, a
//! bare git repository as the remote, and a fake `gh` first on `PATH` that
//! records its arguments and answers with canned JSON. Hermetic: nothing
//! reaches GitHub, and no credential is read. Requires `git` and `sh`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

use branchyard_testkit::fake_agent;
use branchyard_testkit::wait;
use serde_json::{json, Value};

/// A fake `gh`: appends its arguments to `calls.log` (one line per call),
/// saves a body read from stdin, and answers from files in its directory.
/// `NAME.N` files are the Nth answer to a repeated question; the last one
/// given repeats.
const FAKE_GH: &str = r#"#!/bin/sh
d="$FAKE_GH_DIR"
printf '%s\n' "$*" >> "$d/calls.log"
next() {
  n=$(cat "$d/$1.count" 2>/dev/null || echo 0)
  n=$((n + 1))
  echo "$n" > "$d/$1.count"
  if [ -f "$d/$1.$n" ]; then cp "$d/$1.$n" "$d/$1.last"; fi
  cat "$d/$1.last"
}
case "$1 $2" in
"auth status")
  if [ -f "$d/unauthenticated" ]; then
    echo "You are not logged into any GitHub hosts. To log in, run: gh auth login" >&2
    exit 1
  fi
  echo "github.com: Logged in to github.com account tester" >&2 ;;
"issue view") cat "$d/issue.json" ;;
"pr list") if [ -f "$d/created" ]; then cat "$d/pr-list.json"; else echo '[]'; fi ;;
"pr create")
  cat > "$d/body.create"
  touch "$d/created"
  echo "https://github.com/acme/widgets/pull/7" ;;
"pr edit") cat > "$d/body.edit" ;;
"pr view") next view ;;
"pr checks") next checks ;;
"run view") cat "$d/run.log" ;;
"api graphql")
  case "$*" in
  *addPullRequestReviewThreadReply*)
    echo '{"data": {"addPullRequestReviewThreadReply": {"comment": {"id": "R1"}}}}' ;;
  *resolveReviewThread*)
    if [ -f "$d/resolve-fails" ]; then echo "GraphQL: Resource not accessible" >&2; exit 1; fi
    echo '{"data": {"resolveReviewThread": {"thread": {"isResolved": true}}}}' ;;
  *) next threads ;;
  esac ;;
*) echo "fake gh: unexpected: $*" >&2; exit 1 ;;
esac
"#;

/// The kit's repository, plus what this file adds.
struct Repo {
    kit: branchyard_testkit::Repo,
    /// The fake gh's directory: its script, answers and call log.
    gh: PathBuf,
    remote: PathBuf,
}

impl std::ops::Deref for Repo {
    type Target = branchyard_testkit::Repo;
    fn deref(&self) -> &Self::Target {
        &self.kit
    }
}

impl Repo {
    fn new() -> Repo {
        let mut kit = branchyard_testkit::repo!();
        let gh = kit.dir.join("gh");
        fs::create_dir_all(&gh).unwrap();
        kit.set_env(
            "PATH",
            &format!(
                "{}:{}",
                gh.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
        kit.set_env("FAKE_GH_DIR", &gh.display().to_string());
        for var in ["VISUAL", "EDITOR", "GH_TOKEN", "GITHUB_TOKEN"] {
            kit.remove_env(var);
        }
        let repo = Repo {
            gh,
            remote: kit.dir.join("remote.git"),
            kit,
        };
        let script = repo.gh.join("gh");
        fs::write(&script, FAKE_GH).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            repo.gh.join("pr-list.json"),
            r#"[{"number": 7, "url": "https://github.com/acme/widgets/pull/7", "isDraft": false, "baseRefName": "main"}]"#,
        )
        .unwrap();
        let out = repo
            .command("git")
            .args(["init", "-q", "--bare"])
            .arg(&repo.remote)
            .output()
            .unwrap();
        assert!(out.status.success());
        repo.git(&["remote", "add", "origin", repo.remote.to_str().unwrap()]);
        repo
    }

    /// `git` in the bare remote.
    fn remote_git(&self, args: &[&str]) -> Output {
        self.command("git")
            .arg("--git-dir")
            .arg(&self.remote)
            .args(args)
            .output()
            .unwrap()
    }

    /// `by <args>` with the fake agent as the gemini-cli harness.
    fn by_agent(&self, args: &[&str]) -> Output {
        let agent = fake_agent!().display().to_string();
        let mut all: Vec<&str> = args.to_vec();
        all.extend(["--command", &agent]);
        if !matches!(args[0], "send" | "pr") && !args.contains(&"--harness") {
            all.extend(["--harness", "gemini-cli"]);
        }
        self.by(&all)
    }

    fn ok(&self, out: Output) -> String {
        assert!(
            out.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            stdout(&out),
            stderr(&out)
        );
        stdout(&out)
    }

    fn json(&self, args: &[&str]) -> Value {
        let out = self.by(args);
        assert!(out.status.success(), "{}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn answer(&self, name: &str, value: &Value) {
        fs::write(self.gh.join(name), value.to_string()).unwrap();
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.gh.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn count_calls(&self, prefix: &str) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }

    /// The pull-request events of a branch.
    fn pr_events(&self, branch: &str) -> Vec<Value> {
        let events = self.json(&["log", branch, "--json"]);
        events
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["activity"] == "pull_request")
            .map(|e| e["pull_request"].clone())
            .collect()
    }

    fn candidate(&self, branch: &str) -> String {
        let info = self.json(&["show", branch, "--json"]);
        info["candidate"]["commit"].as_str().unwrap().to_owned()
    }

    /// Start the fake gh's numbered answers over.
    fn reset_answers(&self) {
        for entry in fs::read_dir(&self.gh).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let numbered = ["view.", "checks.", "threads."]
                .iter()
                .any(|p| name.starts_with(p));
            if numbered {
                fs::remove_file(path).unwrap();
            }
        }
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn view(state: &str, head: &str) -> Value {
    json!({
        "number": 7,
        "url": "https://github.com/acme/widgets/pull/7",
        "state": state,
        "isDraft": false,
        "headRefOid": head,
        "reviewDecision": "",
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "CLEAN",
        "reviews": [],
        "comments": []
    })
}

fn threads(nodes: Value) -> Value {
    json!({"data": {"repository": {"pullRequest": {"reviewThreads": {"nodes": nodes}}}}})
}

#[test]
fn an_issue_names_the_branch_prompts_it_and_closes_with_its_pull_request() {
    let repo = Repo::new();
    repo.answer(
        "issue.json",
        &json!({
            "number": 12,
            "title": "Parser crash on empty input",
            "body": "The parser panics on an empty file. WRITE issue.txt=fixed",
            "url": "https://github.com/acme/widgets/issues/12",
            "labels": [{"name": "bug"}]
        }),
    );
    let text = repo.ok(repo.by_agent(&[
        "run",
        "--issue",
        "#12",
        "Keep the change small.",
        "--check",
        "true",
    ]));
    assert!(text.contains("wrote issue.txt"), "{text}");
    assert!(repo
        .calls()
        .contains(&"issue view 12 --json number,title,body,url,labels".to_owned()));
    let name = "issue-12-parser-crash-on-empty-input";
    let info = repo.json(&["show", name, "--json"]);
    let prompt = info["prompt"].as_str().unwrap();
    assert!(
        prompt.starts_with(
            "Resolve GitHub issue #12: Parser crash on empty input\n\
             https://github.com/acme/widgets/issues/12\nLabels: bug\n\n\
             The parser panics on an empty file."
        ),
        "{prompt}"
    );
    assert!(
        prompt.ends_with("\nAdditional instructions:\nKeep the change small.\n"),
        "{prompt}"
    );
    assert_eq!(info["merge_readiness"], Value::Null);
    assert_eq!(
        repo.pr_events(name),
        [json!({"kind": "issue_linked", "number": 12,
                "url": "https://github.com/acme/widgets/issues/12",
                "title": "Parser crash on empty input"})]
    );
    let log = stdout(&repo.by(&["log", name]));
    assert!(
        log.contains("issue #12 linked: Parser crash on empty input"),
        "{log}"
    );

    // A second run from the same issue gets its own branch.
    repo.ok(repo.by_agent(&["run", "--issue", "12"]));
    repo.json(&["show", &format!("{name}-2"), "--json"]);

    // by pr: check, push, open.
    let text = repo.ok(repo.by(&["pr", name]));
    assert!(text.contains("check `true` passed"), "{text}");
    assert!(
        text.contains("opened pull request #7: https://github.com/acme/widgets/pull/7"),
        "{text}"
    );
    let candidate = repo.candidate(name);
    let pushed = repo.remote_git(&["rev-parse", &format!("refs/heads/by/{name}")]);
    assert_eq!(stdout(&pushed).trim(), candidate);
    assert!(repo.calls().contains(&format!(
        "pr create --head by/{name} --title Parser crash on empty input --body-file -"
    )));
    let body = fs::read_to_string(repo.gh.join("body.create")).unwrap();
    for expected in [
        "## Task\n\n> Resolve GitHub issue #12: Parser crash on empty input\n",
        "> Keep the change small.\n",
        "\nCloses #12\n",
        &format!("| Branch | `by/{name}` on gemini-cli |"),
        "| Turns | 1 |",
        "| Cost | not reported by the harness |",
        &format!("| Check | `true` passed on `{}` |", &candidate[..10]),
        "| Candidate | ",
        "1 file changed, +1 −0 |",
        "<details><summary>Files changed</summary>",
        "issue.txt | 1 +",
    ] {
        assert!(body.contains(expected), "{expected:?} missing from\n{body}");
    }

    // Again: the same pull request is updated, the check not run again.
    let again = repo.json(&["pr", name, "--json"]);
    assert_eq!(again["created"], false);
    assert_eq!(again["pull_request"]["number"], 7);
    assert_eq!(again["pushed"]["commit"], candidate.as_str());
    assert_eq!(repo.count_calls("pr create"), 1);
    assert_eq!(repo.count_calls("pr edit 7 --body-file -"), 1);
    assert!(fs::read_to_string(repo.gh.join("body.edit"))
        .unwrap()
        .contains("Closes #12"));
    let kinds: Vec<String> = repo
        .pr_events(name)
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        kinds,
        [
            "issue_linked",
            "checked",
            "pushed",
            "opened",
            "pushed",
            "updated"
        ]
    );

    // Until it is observed, readiness says so.
    let show = repo.ok(repo.by(&["show", name]));
    assert!(
        show.contains("merge readiness  unknown (by show --refresh asks GitHub) · check passed · PR #7 not observed yet"),
        "{show}"
    );
}

#[test]
fn watch_sends_ci_failures_and_review_comments_back_once_and_pushes_each_fix() {
    let repo = Repo::new();
    repo.ok(repo.by_agent(&[
        "run",
        "WRITE a.txt=two",
        "--name",
        "feat",
        "--check",
        "true",
    ]));
    repo.ok(repo.by(&["pr", "feat"]));
    let first = repo.candidate("feat");

    let head1 = "1".repeat(40);
    let head2 = "2".repeat(40);
    repo.answer("view.1", &view("OPEN", &head1));
    repo.answer("view.2", &view("OPEN", &head2));
    repo.answer("view.4", &view("MERGED", &head2));
    repo.answer(
        "checks.1",
        &json!([
            {"name": "test", "state": "FAILURE", "bucket": "fail", "workflow": "CI",
             "link": "https://github.com/acme/widgets/actions/runs/99/job/5"},
            {"name": "lint", "state": "SUCCESS", "bucket": "pass", "workflow": "CI",
             "link": "https://github.com/acme/widgets/actions/runs/99/job/6"}
        ]),
    );
    repo.answer(
        "checks.2",
        &json!([{"name": "test", "state": "SUCCESS", "bucket": "pass", "workflow": "CI",
                 "link": "https://github.com/acme/widgets/actions/runs/100/job/7"}]),
    );
    repo.answer("threads.1", &threads(json!([])));
    let comment = threads(json!([
        {"isResolved": false, "path": "a.txt", "line": 1, "comments": {"nodes": [
            {"id": "C1", "author": {"login": "alice"}, "body": "Please WRITE review.txt=done",
             "path": "a.txt", "line": 1, "url": "https://github.com/acme/widgets/pull/7#c1"}
        ]}},
        {"isResolved": true, "path": "a.txt", "line": 1, "comments": {"nodes": [
            {"id": "C0", "author": {"login": "bob"}, "body": "WRITE resolved.txt=no",
             "path": "a.txt", "line": 1, "url": "https://github.com/acme/widgets/pull/7#c0"}
        ]}}
    ]));
    repo.answer("threads.2", &comment);
    fs::write(
        repo.gh.join("run.log"),
        "test\tRun cargo test\tthread 'parser' panicked: expected two\n\
         test\tRun cargo test\thint: WRITE fix.txt=fixed\n",
    )
    .unwrap();

    let text = repo.ok(repo.by_agent(&["pr", "feat", "--watch", "--interval", "50ms", "--yes"]));
    for expected in [
        "updated pull request #7",
        "pull request #7 open, CI 1 passed, 1 failed (test); 0 unresolved threads; mergeable",
        "sending 1 piece of feedback into feat: CI check test failed on 1111111111",
        "wrote fix.txt",
        "sending 1 piece of feedback into feat: review comment by @alice on a.txt:1",
        "wrote review.txt",
        "stopped watching: the pull request was merged",
    ] {
        assert!(text.contains(expected), "{expected:?} missing from\n{text}");
    }
    assert!(!text.contains("resolved.txt"), "{text}");
    assert_eq!(repo.count_calls("run view 99 --log-failed"), 1);

    // Each turn's candidate went to the remote.
    let last = repo.candidate("feat");
    assert_ne!(last, first);
    let pushed = repo.remote_git(&["rev-parse", "refs/heads/by/feat"]);
    assert_eq!(stdout(&pushed).trim(), last);
    for file in ["fix.txt", "review.txt"] {
        let shown = repo.remote_git(&["show", &format!("refs/heads/by/feat:{file}")]);
        assert!(shown.status.success(), "{file} was not pushed");
    }
    assert_eq!(repo.count_calls("pr edit 7"), 3);

    let events = repo.json(&["log", "feat", "--json"]);
    let events = events.as_array().unwrap();
    let prompts: Vec<&str> = events
        .iter()
        .filter(|e| e["activity"] == "prompt")
        .map(|e| e["text"].as_str().unwrap())
        .collect();
    assert_eq!(prompts.len(), 3, "{prompts:?}");
    assert!(prompts[1].starts_with(
        "Feedback on pull request #7 (https://github.com/acme/widgets/pull/7) for this branch:\n\n\
         1. CI check \"test\" (workflow CI) failed on 1111111111: \
         https://github.com/acme/widgets/actions/runs/99/job/5\n\
         The failed steps' log ends:\n```\n"
    ));
    assert!(prompts[1].contains("hint: WRITE fix.txt=fixed"));
    assert!(prompts[2]
        .contains("1. Review comment by @alice on a.txt:1:\n> Please WRITE review.txt=done"));
    let delivered: Vec<&Value> = repo_events_of(events, "feedback_delivered");
    assert_eq!(delivered.len(), 2);
    assert_eq!(delivered[0]["keys"], json!([format!("ci:test:{head1}")]));
    assert_eq!(delivered[0]["via"], "send");
    assert_eq!(delivered[1]["keys"], json!(["thread:C1"]));
    assert_eq!(repo_events_of(events, "watch_stopped").len(), 1);
    let log = stdout(&repo.by(&["log", "feat"]));
    assert!(
        log.contains(
            "pull request feedback sent into the branch (send), 1 piece: CI check test failed"
        ),
        "{log}"
    );
    assert!(log.contains("stopped watching the pull request: the pull request was merged"));

    // Watching again redelivers nothing: what was sent is in the log.
    repo.reset_answers();
    repo.answer("view.1", &view("OPEN", &head1));
    repo.answer("view.2", &view("MERGED", &head1));
    repo.answer(
        "checks.1",
        &json!([{"name": "test", "state": "FAILURE", "bucket": "fail", "workflow": "CI",
                 "link": "https://github.com/acme/widgets/actions/runs/99/job/5"}]),
    );
    repo.answer("threads.1", &comment);
    let text = repo.ok(repo.by_agent(&["pr", "feat", "--watch", "--interval", "50ms", "--yes"]));
    assert!(!text.contains("sending"), "{text}");
    assert!(
        text.contains("stopped watching: the pull request was merged"),
        "{text}"
    );
    assert_eq!(repo.count_calls("run view"), 1);
    let events = repo.json(&["log", "feat", "--json"]);
    assert_eq!(
        repo_events_of(events.as_array().unwrap(), "feedback_delivered").len(),
        2
    );
    let show = repo.json(&["show", "feat", "--json"]);
    assert_eq!(show["merge_readiness"]["verdict"], "merged");
    assert_eq!(show["merge_readiness"]["feedback_rounds"], 2);
}

/// Review threads the watch fed back are answered and resolved once a
/// pushed commit changes their files; others are left alone, and
/// `--no-resolve` leaves them all.
#[test]
fn watch_resolves_the_threads_a_pushed_fix_addressed() {
    for no_resolve in [false, true] {
        let repo = Repo::new();
        repo.ok(repo.by_agent(&["run", "WRITE a.txt=two", "--name", "feat"]));
        repo.ok(repo.by(&["pr", "feat"]));
        let head = "6".repeat(40);
        repo.answer("view.1", &view("OPEN", &head));
        repo.answer("view.2", &view("MERGED", &head));
        repo.answer("checks.1", &json!([]));
        let open = threads(json!([
            {"id": "T1", "isResolved": false, "path": "a.txt", "line": 1, "comments": {"nodes": [
                {"id": "C1", "author": {"login": "alice"}, "body": "Please WRITE a.txt=three",
                 "path": "a.txt", "line": 1}
            ]}},
            {"id": "T2", "isResolved": false, "path": "other.txt", "line": 3, "comments": {"nodes": [
                {"id": "C2", "author": {"login": "bob"}, "body": "Is this file still needed?",
                 "path": "other.txt", "line": 3}
            ]}},
            {"id": "T3", "isResolved": true, "path": "a.txt", "line": 1, "comments": {"nodes": [
                {"id": "C3", "author": {"login": "bob"}, "body": "old", "path": "a.txt", "line": 1}
            ]}}
        ]));
        repo.answer("threads.1", &open);
        let mut args = vec!["pr", "feat", "--watch", "--interval", "50ms", "--yes"];
        if no_resolve {
            args.push("--no-resolve");
        }
        let text = repo.ok(repo.by_agent(&args));
        assert!(text.contains("sending 2 pieces of feedback"), "{text}");
        assert!(text.contains("wrote a.txt"), "{text}");
        let pushed = repo.candidate("feat");
        let mutations: Vec<String> = repo
            .calls()
            .into_iter()
            .filter(|c| c.contains("mutation"))
            .collect();
        let events = repo.json(&["log", "feat", "--json"]);
        let resolved = repo_events_of(events.as_array().unwrap(), "threads_resolved");
        if no_resolve {
            assert!(mutations.is_empty(), "{mutations:?}");
            assert!(resolved.is_empty());
            continue;
        }
        assert!(
            text.contains(&format!(
                "resolved the review thread on a.txt, addressed in {}",
                &pushed[..10]
            )),
            "{text}"
        );
        assert_eq!(mutations.len(), 2, "{mutations:?}");
        assert!(mutations[0].contains("addPullRequestReviewThreadReply"));
        assert!(mutations[0].contains("-f threadId=T1"));
        assert!(mutations[0].ends_with(&format!("-f body=Addressed in {pushed}.")));
        assert!(mutations[1].contains("resolveReviewThread(input: { threadId: $threadId })"));
        assert!(mutations[1].ends_with("-f threadId=T1"));
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0]["commit"], pushed.as_str());
        assert_eq!(
            resolved[0]["threads"],
            json!([{"id": "T1", "path": "a.txt", "replied": true, "resolved": true}])
        );
        let log = stdout(&repo.by(&["log", "feat"]));
        assert!(
            log.contains(&format!(
                "1 review thread addressed in {} resolved (a.txt)",
                &pushed[..10]
            )),
            "{log}"
        );
    }
}

#[test]
fn a_thread_that_cannot_be_resolved_is_recorded_and_the_watch_goes_on() {
    let repo = Repo::new();
    repo.ok(repo.by_agent(&["run", "WRITE a.txt=two", "--name", "feat"]));
    repo.ok(repo.by(&["pr", "feat"]));
    fs::write(repo.gh.join("resolve-fails"), "").unwrap();
    let head = "7".repeat(40);
    repo.answer("view.1", &view("OPEN", &head));
    repo.answer("view.3", &view("MERGED", &head));
    repo.answer("checks.1", &json!([]));
    repo.answer(
        "threads.1",
        &threads(json!([
            {"id": "T1", "isResolved": false, "path": "a.txt", "line": 1, "comments": {"nodes": [
                {"id": "C1", "author": {"login": "alice"}, "body": "Please WRITE a.txt=three",
                 "path": "a.txt", "line": 1}
            ]}}
        ])),
    );
    let out = repo.by_agent(&["pr", "feat", "--watch", "--interval", "50ms", "--yes"]);
    let text = repo.ok(out);
    assert!(
        text.contains("stopped watching: the pull request was merged"),
        "{text}"
    );
    let events = repo.json(&["log", "feat", "--json"]);
    let resolved = repo_events_of(events.as_array().unwrap(), "threads_resolved");
    assert_eq!(resolved.len(), 1);
    let thread = &resolved[0]["threads"][0];
    assert_eq!(
        (thread["replied"].clone(), thread["resolved"].clone()),
        (json!(true), json!(false))
    );
    assert!(thread["error"]
        .as_str()
        .unwrap()
        .contains("Resource not accessible"));
    // One reply and one attempt to resolve, however many polls followed.
    assert_eq!(
        repo.calls()
            .iter()
            .filter(|c| c.contains("mutation"))
            .count(),
        2
    );
    let log = stdout(&repo.by(&["log", "feat"]));
    assert!(log.contains("0 review threads addressed in"), "{log}");
}

/// A child process killed if a test fails while it runs.
struct Killed(std::process::Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn repo_events_of<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| e["activity"] == "pull_request" && e["pull_request"]["kind"] == kind)
        .map(|e| &e["pull_request"])
        .collect()
}

#[test]
fn max_rounds_stops_the_watch_after_that_many_deliveries() {
    let repo = Repo::new();
    repo.ok(repo.by_agent(&["run", "WRITE a.txt=two", "--name", "feat"]));
    repo.ok(repo.by(&["pr", "feat"]));
    repo.answer("view.1", &view("OPEN", &"3".repeat(40)));
    repo.answer(
        "checks.1",
        &json!([{"name": "test", "bucket": "fail", "workflow": "CI", "link": ""}]),
    );
    repo.answer("threads.1", &threads(json!([])));
    let text = repo.ok(repo.by_agent(&[
        "pr",
        "feat",
        "--watch",
        "--interval",
        "50ms",
        "--max-rounds",
        "1",
        "--yes",
    ]));
    assert!(text.contains("sending 1 piece of feedback"), "{text}");
    assert!(
        text.contains("stopped watching: --max-rounds 1 reached"),
        "{text}"
    );
    // The turn's candidate is pushed before stopping.
    let pushed = repo.remote_git(&["rev-parse", "refs/heads/by/feat"]);
    assert_eq!(stdout(&pushed).trim(), repo.candidate("feat"));
}

#[test]
fn show_combines_the_check_pr_ci_threads_and_mergeability() {
    let repo = Repo::new();
    repo.ok(repo.by_agent(&[
        "run",
        "WRITE a.txt=two",
        "--name",
        "feat",
        "--check",
        "true",
    ]));
    let out = repo.by(&["show", "feat", "--refresh"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("feat has no pull request to refresh"),
        "{}",
        stderr(&out)
    );
    repo.ok(repo.by(&["pr", "feat", "--draft", "--base", "main"]));
    assert!(repo.calls().contains(
        &"pr create --head by/feat --title WRITE a.txt=two --body-file - --base main --draft"
            .to_owned()
    ));
    let mut open = view("OPEN", &"4".repeat(40));
    open["mergeable"] = json!("CONFLICTING");
    open["mergeStateStatus"] = json!("DIRTY");
    open["reviewDecision"] = json!("CHANGES_REQUESTED");
    repo.answer("view.1", &open);
    repo.answer(
        "checks.1",
        &json!([
            {"name": "build", "bucket": "pass"},
            {"name": "lint", "bucket": "fail"},
            {"name": "e2e", "bucket": "pending"}
        ]),
    );
    let thread = |id: &str, resolved: bool| {
        json!({"isResolved": resolved, "comments": {"nodes": [
            {"id": id, "author": {"login": "alice"}, "body": "nit"}]}})
    };
    repo.answer(
        "threads.1",
        &threads(json!([
            thread("T1", false),
            thread("T2", false),
            thread("T3", true)
        ])),
    );

    // No network without --refresh.
    let before = repo.calls().len();
    repo.ok(repo.by(&["show", "feat"]));
    assert_eq!(repo.calls().len(), before);

    let text = repo.ok(repo.by(&["show", "feat", "--refresh"]));
    let line = text
        .lines()
        .find(|l| l.starts_with("merge readiness"))
        .unwrap_or_else(|| panic!("no readiness in\n{text}"));
    assert!(
        line.starts_with(
            "merge readiness  not ready: 1 CI check failed, CI pending, 2 unresolved threads, \
             conflicts with its base, changes requested · check passed · PR #7 open · \
             CI 1 passed, 1 failed (lint), 1 pending · 2 unresolved threads · conflicts · \
             changes requested (observed "
        ),
        "{line}"
    );
    assert_eq!(repo.count_calls("pr view 7"), 1);
    assert_eq!(
        repo.count_calls("api graphql -F owner=acme -F name=widgets -F number=7"),
        1
    );

    let value = repo.json(&["show", "feat", "--json"]);
    let readiness = &value["merge_readiness"];
    assert_eq!(readiness["verdict"], "not_ready");
    assert_eq!(readiness["local_check"]["state"], "passed");
    assert_eq!(readiness["local_check"]["argv"], json!(["true"]));
    assert_eq!(readiness["pull_request"]["number"], 7);
    assert_eq!(readiness["pull_request"]["draft"], true);
    assert_eq!(readiness["pushed"]["remote"], "origin");
    let observation = &readiness["observation"];
    assert_eq!(observation["state"], "open");
    assert_eq!(observation["mergeable"], "conflicting");
    assert_eq!(observation["merge_state"], "dirty");
    assert_eq!(observation["review_decision"], "changes_requested");
    assert_eq!(
        observation["ci"],
        json!({"passed": 1, "failed": 1, "pending": 1, "skipped": 0, "failing": ["lint"]})
    );
    assert_eq!(observation["unresolved_threads"], 2);
    assert!(readiness["observed_at_ms"].as_u64().is_some());
    assert!(readiness["blockers"]
        .as_array()
        .unwrap()
        .contains(&json!("conflicts with its base")));

    // Everything clear: ready.
    repo.reset_answers();
    repo.answer("view.1", &view("OPEN", &"4".repeat(40)));
    repo.answer("checks.1", &json!([{"name": "build", "bucket": "pass"}]));
    repo.answer("threads.1", &threads(json!([thread("T1", true)])));
    let text = repo.ok(repo.by(&["show", "feat", "--refresh"]));
    assert!(
        text.contains("merge readiness  ready to merge · check passed · PR #7 open · CI 1 passed · 0 unresolved threads · mergeable"),
        "{text}"
    );
}

#[test]
fn a_failing_check_or_an_unready_branch_is_not_pushed_without_an_override() {
    let repo = Repo::new();
    repo.ok(repo.by_agent(&[
        "run",
        "WRITE a.txt=two",
        "--name",
        "feat",
        "--check",
        "false",
    ]));
    let out = repo.by(&["pr", "feat"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("the check `false` failed on the branch's candidate"),
        "{}",
        stderr(&out)
    );
    assert!(!repo
        .remote_git(&["rev-parse", "--verify", "refs/heads/by/feat"])
        .status
        .success());
    assert_eq!(repo.count_calls("pr create"), 0);
    let text = repo.ok(repo.by(&["pr", "feat", "--allow-failing-check"]));
    assert!(text.contains("check `false` failed"), "{text}");
    assert!(text.contains("opened pull request #7"), "{text}");
    let body = fs::read_to_string(repo.gh.join("body.create")).unwrap();
    assert!(body.contains("`false` **failed** on"), "{body}");

    // A branch whose turn failed has a candidate only with --allow-not-ready.
    repo.ok(repo.by_agent(&["run", "WRITE b.txt=x", "--name", "second"]));
    let out = repo.by_agent(&["send", "second", "EXIT"]);
    assert!(!out.status.success());
    let out = repo.by(&["pr", "second", "--no-check"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("pass --allow-not-ready"),
        "{}",
        stderr(&out)
    );
    repo.ok(repo.by(&[
        "pr",
        "second",
        "--no-check",
        "--allow-not-ready",
        "--head",
        "second",
    ]));
    assert!(repo
        .remote_git(&["rev-parse", "--verify", "refs/heads/second"])
        .status
        .success());
}

#[test]
fn missing_or_logged_out_gh_is_explained_before_anything_is_pushed() {
    let repo = Repo::new();
    repo.ok(repo.by_agent(&["run", "WRITE a.txt=two", "--name", "feat"]));

    // No gh at all: a PATH with only git on it.
    let bin = repo.dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let git = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    std::os::unix::fs::symlink(git.trim(), bin.join("git")).unwrap();
    let out = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .args(["pr", "feat"])
        .env("PATH", &bin)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("needs the GitHub CLI, gh, which is not on PATH")
            && stderr(&out).contains("https://cli.github.com"),
        "{}",
        stderr(&out)
    );
    let out = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .args(["run", "--issue", "3", "--harness", "gemini-cli"])
        .env("PATH", &bin)
        .output()
        .unwrap();
    assert!(stderr(&out).contains("not on PATH"), "{}", stderr(&out));

    // Logged out.
    fs::write(repo.gh.join("unauthenticated"), "").unwrap();
    let out = repo.by(&["pr", "feat"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("gh is not logged in to GitHub; run `gh auth login` first"),
        "{}",
        stderr(&out)
    );
    let out = repo.by_agent(&["run", "--issue", "3"]);
    assert!(stderr(&out).contains("gh auth login"), "{}", stderr(&out));
    assert_eq!(repo.calls(), ["auth status", "auth status"]);
    assert!(!repo
        .remote_git(&["rev-parse", "--verify", "refs/heads/by/feat"])
        .status
        .success());
}

#[test]
fn open_starts_the_editor_on_the_worktree() {
    let repo = Repo::new();
    repo.ok(repo.by_agent(&["run", "WRITE a.txt=two", "--name", "feat"]));
    let worktree = repo.json(&["show", "feat", "--json"])["worktree"]
        .as_str()
        .unwrap()
        .to_owned();
    let editor = repo.dir.join("fake-editor");
    let opened = repo.dir.join("opened");
    fs::write(
        &editor,
        format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\n", opened.display()),
    )
    .unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();

    let editor_line = format!("{} --wait", editor.display());
    repo.ok(repo.by(&["open", "feat", "--editor", &editor_line]));
    assert_eq!(
        fs::read_to_string(&opened).unwrap(),
        format!("--wait\n{worktree}\n")
    );
    fs::remove_file(&opened).unwrap();
    let out = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .args(["open", "feat"])
        .env("EDITOR", &editor)
        .output()
        .unwrap();
    repo.ok(out);
    assert_eq!(
        fs::read_to_string(&opened).unwrap(),
        format!("{worktree}\n")
    );

    let printed = repo.ok(repo.by(&["open", "feat", "--print"]));
    assert_eq!(printed, format!("{worktree}\n"));

    let out = repo.by(&["open", "feat"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no editor: set $VISUAL or $EDITOR"),
        "{}",
        stderr(&out)
    );
    let out = repo.by(&["open", "feat", "--editor", "zed"]);
    if !out.status.success() {
        assert!(
            stderr(&out).contains("zed (from --editor) is not on PATH"),
            "{}",
            stderr(&out)
        );
    }
    let out = repo.by(&["open", "nothing", "--print"]);
    assert!(!out.status.success());
}

#[test]
fn remote_mode_refuses_pr_open_and_refresh_by_name() {
    let repo = Repo::new();
    let token = repo.dir.join("token");
    fs::write(&token, "secret\n").unwrap();
    let token = token.display().to_string();
    let remote = [
        "--remote",
        "http://127.0.0.1:9",
        "--token-file",
        token.as_str(),
        "--repo",
        "r",
    ];
    for (command, expected) in [
        (vec!["pr", "feat"], "by pr works in local mode only"),
        (vec!["open", "feat"], "by open works in local mode only"),
        (
            vec!["show", "feat", "--refresh"],
            "it works in local mode only",
        ),
    ] {
        let mut args: Vec<&str> = remote.to_vec();
        args.extend(command);
        let out = repo.by(&args);
        assert!(!out.status.success());
        assert!(
            stderr(&out).contains(expected),
            "{args:?}: {}",
            stderr(&out)
        );
    }
    assert!(repo.calls().is_empty());
}

#[test]
fn pr_is_refused_inside_a_harness_and_its_watch_flags_need_watch() {
    let repo = Repo::new();
    let out = repo
        .command(env!("CARGO_BIN_EXE_by"))
        .args(["pr", "feat"])
        .env("BRANCHYARD_BRANCH", "feat")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("does not run inside a harness"),
        "{}",
        stderr(&out)
    );
    let out = repo.by(&["pr", "feat", "--yes"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("add --watch"), "{}", stderr(&out));
    let out = repo.by(&["pr", "feat", "--max-rounds", "2"]);
    assert_eq!(out.status.code(), Some(2));
    let out = repo.by(&["pr", "feat", "--watch", "--json"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn watch_steers_feedback_into_a_running_turn() {
    let repo = Repo::new();
    repo.ok(repo.by_agent(&["run", "WRITE a.txt=two", "--name", "feat"]));
    repo.ok(repo.by(&["pr", "feat"]));
    // A turn someone else started, still running when feedback arrives.
    let agent = fake_agent!().display().to_string();
    let mut running = Killed(
        repo.command(env!("CARGO_BIN_EXE_by"))
            .args(["send", "feat", "AWAIT_STEER", "--command", &agent, "--yes"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    wait::until("the turn to start", || {
        let log = stdout(&repo.by(&["log", "feat"]));
        if log.contains("waiting for steering") {
            Ok(())
        } else {
            Err(log)
        }
    });
    let mut open = view("OPEN", &"5".repeat(40));
    open["comments"] = json!([
        {"id": "IC_1", "author": {"login": "carol"}, "body": "Rename the flag, please."}
    ]);
    repo.answer("view.1", &open);
    repo.answer("view.2", &view("MERGED", &"5".repeat(40)));
    repo.answer("checks.1", &json!([]));
    repo.answer("threads.1", &threads(json!([])));
    let text = repo.ok(repo.by(&["pr", "feat", "--watch", "--interval", "50ms"]));
    assert!(
        text.contains("sending 1 piece of feedback into feat: comment by @carol"),
        "{text}"
    );
    assert!(text.contains("steered into feat's running turn"), "{text}");
    assert!(running.0.wait().unwrap().success());

    let events = repo.json(&["log", "feat", "--json"]);
    let events = events.as_array().unwrap();
    let delivered = repo_events_of(events, "feedback_delivered");
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0]["via"], "steer");
    assert_eq!(delivered[0]["keys"], json!(["comment:IC_1"]));
    let steered: Vec<&Value> = events
        .iter()
        .filter(|e| e["activity"] == "steered")
        .collect();
    assert_eq!(steered.len(), 1);
    assert_eq!(steered[0]["by"], "by pr --watch");
    assert!(steered[0]["text"]
        .as_str()
        .unwrap()
        .contains("@carol commented:\n> Rename the flag, please."));
    // No new turn was started for it.
    assert_eq!(
        events.iter().filter(|e| e["activity"] == "prompt").count(),
        2
    );
}
