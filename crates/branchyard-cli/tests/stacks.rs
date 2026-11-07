//! Stacked branches and pull requests, end to end with the built `by`, the
//! fake ACP agent, a bare git repository as the remote and a fake `gh` that
//! keeps a little state (pull request numbers, bases) and records its calls.
//! Hermetic: nothing reaches GitHub. Requires `git` and `sh`.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;

use branchyard_testkit::fake_agent;
use serde_json::{json, Value};

/// A fake `gh`. `pr create` numbers pull requests from 7 and remembers each
/// head's number and base; `pr edit --base` updates the base; `pr view N`
/// answers `view-N.json`, or the numbered `view-N.K` files in turn (the last
/// one repeats) when `view-N.1` exists. Every call is appended to
/// `calls.log`.
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
after() {
  flag="$1"; shift
  while [ $# -gt 0 ]; do
    if [ "$1" = "$flag" ]; then echo "$2"; return; fi
    shift
  done
}
case "$1 $2" in
"auth status") echo "github.com: Logged in" >&2 ;;
"pr list")
  head=$(after --head "$@" | tr '/' '_')
  if [ -f "$d/list.$head.json" ]; then cat "$d/list.$head.json"; else echo '[]'; fi ;;
"pr create")
  head=$(after --head "$@" | tr '/' '_')
  base=$(after --base "$@")
  [ -n "$base" ] || base=main
  n=$(cat "$d/seq" 2>/dev/null || echo 6)
  n=$((n + 1))
  echo "$n" > "$d/seq"
  cat > "$d/body.create.$n"
  printf '[{"number": %s, "url": "https://github.com/acme/widgets/pull/%s", "isDraft": false, "baseRefName": "%s"}]\n' \
    "$n" "$n" "$base" > "$d/list.$head.json"
  echo "https://github.com/acme/widgets/pull/$n" ;;
"pr edit")
  n="$3"
  cat > "$d/body.edit.$n"
  base=$(after --base "$@")
  if [ -n "$base" ]; then
    for f in "$d"/list.*.json; do
      if grep -q "\"number\": $n," "$f"; then
        sed "s/\"baseRefName\": \"[^\"]*\"/\"baseRefName\": \"$base\"/" "$f" > "$f.tmp"
        mv "$f.tmp" "$f"
      fi
    done
  fi ;;
"pr view")
  if [ -f "$d/view-$3.1" ]; then next "view-$3"; else cat "$d/view-$3.json"; fi ;;
"pr checks") echo '[]' ;;
"api graphql")
  echo '{"data": {"repository": {"pullRequest": {"reviewThreads": {"nodes": []}}}}}' ;;
*) echo "fake gh: unexpected: $*" >&2; exit 1 ;;
esac
"#;

struct Repo {
    kit: branchyard_testkit::Repo,
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
            remote: kit.dir.join("remote.git"),
            gh,
            kit,
        };
        let script = repo.gh.join("gh");
        fs::write(&script, FAKE_GH).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let out = repo
            .command("git")
            .args(["init", "-q", "--bare"])
            .arg(&repo.remote)
            .output()
            .unwrap();
        assert!(out.status.success());
        repo.git(&["remote", "add", "origin", repo.remote.to_str().unwrap()]);
        repo.git(&["push", "-q", "origin", "HEAD:refs/heads/main"]);
        repo
    }

    /// `by <args>` with the fake agent as the gemini-cli harness.
    fn by_agent(&self, args: &[&str]) -> Output {
        let agent = fake_agent!().display().to_string();
        let mut all: Vec<&str> = args.to_vec();
        all.extend(["--command", &agent, "--allow-unapproved-tools"]);
        if !matches!(args[0], "send" | "pr") && !args.contains(&"--harness") {
            all.extend(["--harness", "gemini-cli"]);
        }
        self.by(&all)
    }

    fn ok(&self, out: Output) -> String {
        assert!(
            out.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn show(&self, branch: &str) -> Value {
        self.json(&["show", branch, "--json"])
    }

    fn candidate(&self, branch: &str) -> String {
        self.show(branch)["candidate"]["commit"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn answer(&self, name: &str, value: &Value) {
        fs::write(self.gh.join(name), value.to_string()).unwrap();
    }

    fn file(&self, name: &str) -> String {
        fs::read_to_string(self.gh.join(name)).unwrap()
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.gh.join("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn pr_events(&self, branch: &str) -> Vec<Value> {
        self.json(&["log", branch, "--json"])
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["activity"] == "pull_request")
            .map(|e| e["pull_request"].clone())
            .collect()
    }

    /// The remote's `main` moves on as a squash merge of the parent's pull
    /// request would move it: one new commit with the parent's change, but
    /// not the parent's commits. `files` are written (path, content).
    fn squash_into_main(&self, files: &[(&str, &str)]) -> String {
        let clone = self.kit.dir.join("squash");
        let ok = |out: Output| assert!(out.status.success(), "{out:?}");
        ok(self
            .command("git")
            .args(["clone", "-q", "-b", "main"])
            .arg(&self.remote)
            .arg(&clone)
            .output()
            .unwrap());
        for (path, content) in files {
            fs::write(clone.join(path), content).unwrap();
        }
        for args in [
            vec!["add", "--all"],
            vec![
                "-c",
                "user.name=merger",
                "-c",
                "user.email=merger@example.com",
                "commit",
                "-q",
                "-m",
                "Squash merge of the parent (#7)",
            ],
            vec!["push", "-q", "origin", "HEAD:refs/heads/main"],
        ] {
            ok(self
                .command("git")
                .arg("-C")
                .arg(&clone)
                .args(args)
                .output()
                .unwrap());
        }
        let main = self
            .command("git")
            .arg("--git-dir")
            .arg(&self.remote)
            .args(["rev-parse", "refs/heads/main"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&main.stdout).trim().to_owned()
    }

    fn remote_ref(&self, name: &str) -> String {
        let out = self
            .command("git")
            .arg("--git-dir")
            .arg(&self.remote)
            .args(["rev-parse", &format!("refs/heads/{name}")])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }
}

fn view(state: &str, base: &str) -> Value {
    json!({"state": state, "baseRefName": base})
}

fn full_view(state: &str) -> Value {
    json!({
        "number": 8,
        "url": "https://github.com/acme/widgets/pull/8",
        "state": state,
        "isDraft": false,
        "headRefOid": "1".repeat(40),
        "reviewDecision": "",
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "CLEAN",
        "reviews": [],
        "comments": []
    })
}

/// A parent with its pull request (#7) and a child stacked on it.
fn parent_and_child(repo: &Repo, child_writes: &str) {
    repo.answer("view-7.json", &view("OPEN", "main"));
    repo.ok(repo.by_agent(&[
        "run",
        "WRITE a.txt=parent",
        "--name",
        "parent",
        "--check",
        "true",
    ]));
    repo.ok(repo.by(&["pr", "parent"]));
    repo.ok(repo.by_agent(&[
        "run",
        child_writes,
        "--name",
        "child",
        "--base",
        "parent",
        "--check",
        "true",
    ]));
}

#[test]
fn a_child_is_opened_against_its_parent_then_rebased_and_retargeted_when_it_merges() {
    let repo = Repo::new();
    parent_and_child(&repo, "WRITE b.txt=child");

    // The child starts on the parent's candidate and records the stack.
    let parent_candidate = repo.candidate("parent");
    let child = repo.show("child");
    assert_eq!(child["base"], parent_candidate.as_str());
    assert_eq!(child["stack"]["parent"], "parent");
    assert_eq!(child["stack"]["base"], parent_candidate.as_str());
    let parent = repo.show("parent");
    assert_eq!(parent["stack"]["children"], json!(["child"]));
    let text = String::from_utf8_lossy(&repo.by(&["show", "child"]).stdout).into_owned();
    assert!(
        text.contains("stack") && text.contains("on parent (pull request #7)"),
        "{text}"
    );
    assert_eq!(
        repo.pr_events("child")[0],
        json!({"kind": "stacked", "parent": "parent", "base": parent_candidate})
    );

    // Its pull request targets the parent's head and lists the stack.
    let text = repo.ok(repo.by(&["pr", "child"]));
    assert!(text.contains("opened pull request #8"), "{text}");
    let create = repo
        .calls()
        .into_iter()
        .find(|c| c.starts_with("pr create --head by/child"))
        .unwrap();
    assert!(create.ends_with("--base by/parent"), "{create}");
    let body = repo.file("body.create.8");
    for expected in [
        "## Stack\n",
        "1. https://github.com/acme/widgets/pull/7 `parent`\n",
        "2. **this pull request** (`child`)\n",
    ] {
        assert!(body.contains(expected), "{expected:?} missing from\n{body}");
    }
    // The parent's body, regenerated, lists the child below it.
    repo.ok(repo.by(&["pr", "parent"]));
    let parent_body = repo.file("body.edit.7");
    assert!(
        parent_body.contains("2. https://github.com/acme/widgets/pull/8 `child`"),
        "{parent_body}"
    );

    // While the parent is open, by pr only updates.
    repo.ok(repo.by(&["pr", "child"]));
    assert!(!repo
        .calls()
        .iter()
        .any(|c| c.starts_with("pr edit 8") && c.contains("--base")));

    // The parent merges (squashed): the child is rebased onto main, pushed
    // over its old head, and its pull request retargeted.
    let before = repo.candidate("child");
    let main = repo.squash_into_main(&[("a.txt", "parent, squashed\n")]);
    repo.answer("view-7.json", &view("MERGED", "main"));
    let text = repo.ok(repo.by(&["pr", "child"]));
    assert!(text.contains("updated pull request #8"), "{text}");
    let after = repo.candidate("child");
    assert_ne!(before, after);
    assert_eq!(repo.remote_ref("by/child"), after);
    repo.git(&["merge-base", "--is-ancestor", &main, &after]);
    let info = repo.show("child");
    assert_eq!(info["base"], main.as_str());
    assert_eq!(info["stack"]["parent"], Value::Null);
    assert_eq!(info["stack"]["restacked"]["onto"], "main");
    assert!(repo
        .calls()
        .contains(&"pr edit 8 --body-file - --base main".to_owned()));
    let worktree = PathBuf::from(info["worktree"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(worktree.join("a.txt")).unwrap(),
        "parent, squashed\n"
    );
    assert_eq!(
        fs::read_to_string(worktree.join("b.txt")).unwrap().trim(),
        "child"
    );
    let restacked: Vec<Value> = repo
        .pr_events("child")
        .into_iter()
        .filter(|e| e["kind"] == "restacked")
        .collect();
    assert_eq!(restacked.len(), 1);
    assert_eq!(restacked[0]["parent"], "parent");
    assert_eq!(restacked[0]["onto_commit"], main.as_str());
    assert_eq!(restacked[0]["from"], before.as_str());
    assert_eq!(restacked[0]["to"], after.as_str());
    let log = String::from_utf8_lossy(&repo.by(&["log", "child"]).stdout).into_owned();
    assert!(
        log.contains("parent's pull request merged: rebased onto main"),
        "{log}"
    );

    // Once is enough: the next by pr changes nothing more.
    repo.ok(repo.by(&["pr", "child"]));
    assert_eq!(repo.candidate("child"), after);
    assert_eq!(
        repo.calls()
            .iter()
            .filter(|c| c.starts_with("pr edit 8") && c.contains("--base"))
            .count(),
        1
    );
}

#[test]
fn a_conflicting_rebase_is_reported_and_changes_nothing() {
    let repo = Repo::new();
    parent_and_child(&repo, "WRITE a.txt=child");
    repo.ok(repo.by(&["pr", "child"]));
    let before = repo.candidate("child");
    let pushed = repo.remote_ref("by/child");
    repo.squash_into_main(&[("a.txt", "something else entirely\n")]);
    repo.answer("view-7.json", &view("MERGED", "main"));

    let out = repo.by(&["pr", "child"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        err.contains("does not rebase onto it cleanly") && err.contains("a.txt"),
        "{err}"
    );
    assert!(
        err.contains("next:") && err.contains("by send child"),
        "{err}"
    );
    assert_eq!(repo.candidate("child"), before);
    assert_eq!(repo.remote_ref("by/child"), pushed);
    assert!(!repo
        .calls()
        .iter()
        .any(|c| c.starts_with("pr edit 8") && c.contains("--base main")));
    let blocked = repo
        .pr_events("child")
        .into_iter()
        .find(|e| e["kind"] == "restack_blocked")
        .unwrap();
    assert_eq!(blocked["parent"], "parent");
    assert_eq!(blocked["onto"], "main");
    assert_eq!(blocked["files"], json!(["a.txt"]));
    // The child is still stacked, and the worktree is not left mid-rebase.
    let info = repo.show("child");
    assert_eq!(info["stack"]["parent"], "parent");
    let worktree = PathBuf::from(info["worktree"].as_str().unwrap());
    let status = repo
        .command("git")
        .arg("-C")
        .arg(&worktree)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(status.stdout.is_empty(), "{status:?}");
}

#[test]
fn watch_follows_the_parent_and_restacks_when_it_merges() {
    let repo = Repo::new();
    parent_and_child(&repo, "WRITE b.txt=child");
    repo.ok(repo.by(&["pr", "child"]));
    let before = repo.candidate("child");
    let main = repo.squash_into_main(&[("a.txt", "parent, squashed\n")]);
    // The parent is open for the publish and the first poll, merged at the
    // second; the child's own pull request is open, then merged.
    let open = view("OPEN", "main");
    let merged = view("MERGED", "main");
    repo.answer("view-7.1", &open);
    repo.answer("view-7.2", &open);
    repo.answer("view-7.3", &merged);
    repo.answer("view-8.1", &full_view("OPEN"));
    repo.answer("view-8.2", &full_view("MERGED"));
    fs::remove_file(repo.gh.join("view-7.json")).unwrap();

    let text = repo.ok(repo.by(&["pr", "child", "--watch", "--interval", "50ms"]));
    assert!(
        text.contains("stopped watching: the pull request was merged"),
        "{text}"
    );
    let after = repo.candidate("child");
    assert_ne!(before, after);
    assert_eq!(repo.remote_ref("by/child"), after);
    repo.git(&["merge-base", "--is-ancestor", &main, &after]);
    assert!(repo
        .calls()
        .contains(&"pr edit 8 --body-file - --base main".to_owned()));
    let kinds: Vec<String> = repo
        .pr_events("child")
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_owned())
        .collect();
    assert!(kinds.contains(&"restacked".to_owned()), "{kinds:?}");
}

#[test]
fn a_child_of_a_fork_branch_waits_for_that_branch_to_have_a_pull_request() {
    let repo = Repo::new();
    let head = repo.git(&["rev-parse", "HEAD"]).trim().to_owned();
    repo.answer(
        "view-5.json",
        &json!({
            "number": 5, "title": "Fork change", "body": "from a fork",
            "url": "https://github.com/other/widgets/pull/5",
            "headRefName": "feature", "headRefOid": head, "baseRefName": "main",
            "isCrossRepository": true, "state": "OPEN"
        }),
    );
    repo.ok(repo.by_agent(&["run", "--pr", "5", "WRITE f.txt=fork"]));
    // A fork's head is not ours to push to: the branch has no pull request
    // of its own until by pr opens a new one.
    let parent = "pr-5-fork-change";
    assert_eq!(repo.pr_events(parent), Vec::<Value>::new());
    repo.ok(repo.by_agent(&[
        "run",
        "WRITE g.txt=child",
        "--name",
        "child",
        "--base",
        parent,
    ]));
    assert_eq!(repo.show("child")["stack"]["parent"], parent);
    let out = repo.by(&["pr", "child", "--no-check"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        err.contains("has no pull request yet") && err.contains(&format!("by pr {parent}")),
        "{err}"
    );
    // Nothing was pushed for the child.
    let refs = repo
        .command("git")
        .arg("--git-dir")
        .arg(&repo.remote)
        .args(["for-each-ref", "refs/heads/by"])
        .output()
        .unwrap();
    assert!(refs.stdout.is_empty(), "{refs:?}");
}
