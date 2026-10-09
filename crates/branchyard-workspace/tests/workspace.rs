//! Hermetic tests against temporary repositories. Requires `git` and `sh`.

#![allow(clippy::let_underscore_must_use, clippy::unwrap_used)] // tests: a panic is the failure report
use branchyard_testkit::wait;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;
use std::time::{Duration, Instant};

use branchyard_workspace::{
    BranchName, Candidate, Check, CheckResult, Commit, GitError, IntegrationError, Repository,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);
static HERMETIC: Once = Once::new();

/// A temporary directory holding a repository (`repo/`), workspaces, and the
/// integration scratch directory. Removed on drop.
struct Fixture {
    dir: PathBuf,
    repo: Repository,
}

impl Fixture {
    fn new() -> Self {
        // Library calls inherit the process environment: keep the host's
        // global and system git configuration out of the tests.
        HERMETIC.call_once(|| {
            std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        });
        let dir = std::env::temp_dir().join(format!(
            "branchyard-workspace-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("repo")).unwrap();
        fs::create_dir_all(dir.join("scratch")).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let root = dir.join("repo");
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "user.email", "test@localhost"]);
        git(&root, &["config", "commit.gpgsign", "false"]);
        fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        fs::write(root.join(".gitignore"), "*.log\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        let mut repo = Repository::open(&root).unwrap();
        repo.set_scratch_dir(dir.join("scratch"));
        Fixture { dir, repo }
    }

    fn root(&self) -> PathBuf {
        self.dir.join("repo")
    }

    fn head(&self, rev: &str) -> Commit {
        self.repo.resolve(rev).unwrap()
    }

    fn workspace(&self, name: &str) -> branchyard_workspace::Workspace {
        let base = self.head("main");
        self.repo
            .create_branch(&name.parse().unwrap(), &base, &self.dir.join(name))
            .unwrap()
    }

    /// A candidate on `by/<name>` that writes `file`.
    fn candidate(&self, name: &str, file: &str, contents: &str) -> Candidate {
        let ws = self.workspace(name);
        fs::write(ws.path.join(file), contents).unwrap();
        ws.snapshot(&format!("write {file}")).unwrap().unwrap()
    }

    /// Commits `contents` to `file` on the checked-out main branch.
    fn commit_on_main(&self, file: &str, contents: &str) -> Commit {
        fs::write(self.root().join(file), contents).unwrap();
        git(&self.root(), &["add", file]);
        git(
            &self.root(),
            &["commit", "-q", "-m", &format!("main: {file}")],
        );
        self.head("main")
    }

    fn assert_no_integration_worktrees(&self) {
        let list = git(&self.root(), &["worktree", "list", "--porcelain"]);
        assert!(!list.contains("branchyard-integrate"), "{list}");
        let left: Vec<_> = fs::read_dir(self.dir.join("scratch")).unwrap().collect();
        assert!(left.is_empty(), "scratch not empty: {left:?}");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn sh(script: &str, timeout: Duration) -> Check {
    Check {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        timeout,
    }
}

#[test]
fn branch_names_are_validated() {
    let fixture = Fixture::new();
    for good in ["a", "fix-parser", "task/42", "v1.2_x", "a/b/c-d", "0"] {
        let name = BranchName::new(good).unwrap();
        assert_eq!(name.branch(), format!("by/{good}"));
        assert_eq!(name.ref_name(), format!("refs/heads/by/{good}"));
        git(
            &fixture.root(),
            &["check-ref-format", "--branch", &name.branch()],
        );
    }
    for bad in [
        "", "Upper", "-flag", ".hidden", "a..b", "a/", "/a", "a//b", "x.lock", "dot.", "a b",
        "a~1", "a^", "a:b", "a?", "a*", "a[b", "a\\b", "a@{1}", "@", "é",
    ] {
        assert!(BranchName::new(bad).is_err(), "{bad:?} accepted");
    }
    assert!(BranchName::new("x".repeat(101)).is_err());
    assert_eq!(
        BranchName::from_branch("refs/heads/by/task/1")
            .unwrap()
            .as_str(),
        "task/1"
    );
    assert!(BranchName::from_branch("main").is_none());
}

#[test]
fn open_resolve_and_current_branch() {
    let fixture = Fixture::new();
    assert!(matches!(
        Repository::open(&fixture.dir.join("scratch")),
        Err(GitError::NotAWorkTree(_))
    ));
    let sub = fixture.root().join("sub");
    fs::create_dir(&sub).unwrap();
    let repo = Repository::open(&sub).unwrap();
    assert_eq!(repo.root(), fixture.root());

    let head = fixture.head("HEAD");
    assert_eq!(head.as_str().len(), 40);
    assert_eq!(fixture.head("main"), head);
    assert!(matches!(
        fixture.repo.resolve("--all"),
        Err(GitError::InvalidRevision(_))
    ));
    assert!(fixture.repo.resolve("nope").is_err());
    assert_eq!(
        fixture.repo.current_branch().unwrap().as_deref(),
        Some("main")
    );
    assert_eq!(
        branchyard_workspace::git::current_branch(&sub)
            .unwrap()
            .as_deref(),
        Some("main")
    );
    git(&fixture.root(), &["checkout", "-q", "--detach"]);
    assert_eq!(fixture.repo.current_branch().unwrap(), None);
    assert_eq!(
        branchyard_workspace::git::current_branch(&fixture.root()).unwrap(),
        None
    );
}

/// The shared `Git` choke point: stdout, a failure's arguments, exit code
/// and stderr, yes/no answers, and a directory that is not there.
#[test]
fn the_git_runner_reports_output_and_failures() {
    use branchyard_workspace::Git;
    let fixture = Fixture::new();
    let root = &fixture.root();
    let out = Git::new(root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .run();
    assert_eq!(out.unwrap(), "main\n");
    match Git::new(root).args(["rev-parse", "--verify", "nope"]).run() {
        Err(GitError::Failed { args, code, stderr }) => {
            assert_eq!(args, ["rev-parse", "--verify", "nope"]);
            assert_eq!(code, Some(128));
            assert!(!stderr.is_empty());
        }
        other => panic!("{other:?}"),
    }
    let exists = |r: &str| {
        Git::new(root)
            .args(["show-ref", "--verify", "--quiet", r])
            .succeeds()
            .unwrap()
    };
    assert!(exists("refs/heads/main"));
    assert!(!exists("refs/heads/nope"));
    // The variables that would point git elsewhere are not inherited.
    let dir = Git::new(root)
        .env("GIT_TEST_UNUSED", "1")
        .args(["rev-parse", "--show-toplevel"])
        .run()
        .unwrap();
    assert_eq!(Path::new(dir.trim()), root);
    assert!(matches!(
        Git::new(&fixture.dir.join("missing")).arg("status").run(),
        Err(GitError::Spawn(_))
    ));
}

/// Input on stdin and raw stdout bytes, as `by try` hashes, reads and
/// applies through the choke point.
#[test]
fn the_git_runner_feeds_stdin_and_returns_bytes() {
    use branchyard_workspace::Git;
    let fixture = Fixture::new();
    let root = &fixture.root();
    let bytes: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
    let blob = Git::new(root)
        .args(["hash-object", "-w", "--no-filters", "--stdin"])
        .stdin(bytes.clone())
        .run()
        .unwrap();
    let back = Git::new(root)
        .args(["cat-file", "blob", blob.trim()])
        .run_bytes()
        .unwrap();
    assert_eq!(back, bytes);
    // A failing command that stops reading its input early is a failure,
    // not a broken pipe.
    match Git::new(root)
        .args(["apply", "--check"])
        .stdin("not a patch\n".repeat(50_000))
        .run()
    {
        Err(GitError::Failed { args, .. }) => assert_eq!(args, ["apply", "--check"]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn create_branch_snapshot_and_diff() {
    let fixture = Fixture::new();
    let base = fixture.head("main");
    let ws = fixture.workspace("feat");
    assert_eq!(ws.base, base);
    assert_eq!(ws.path, fixture.dir.join("feat"));
    assert_eq!(
        git(&ws.path, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/by/feat"
    );
    assert!(matches!(
        fixture
            .repo
            .create_branch(&ws.name, &base, &fixture.dir.join("again")),
        Err(GitError::BranchExists(_))
    ));

    fs::write(ws.path.join("a.txt"), "one\nTWO\nthree\n").unwrap();
    fs::write(ws.path.join("new.txt"), "fresh\n").unwrap();
    fs::write(ws.path.join("debug.log"), "ignored\n").unwrap();

    let diff = ws.diff().unwrap();
    assert!(diff.contains("diff --git a/a.txt b/a.txt"), "{diff}");
    assert!(diff.contains("-two\n+TWO"), "{diff}");
    assert!(diff.contains("+++ b/new.txt"), "{diff}");
    assert!(!diff.contains("debug.log"), "{diff}");
    let stat = ws.diffstat().unwrap();
    assert_eq!(
        (stat.files_changed, stat.insertions, stat.deletions),
        (2, 2, 1)
    );
    // diff/diffstat do not stage anything.
    let status = git(&ws.path, &["status", "--porcelain"]);
    assert!(
        status.contains(" M a.txt") && status.contains("?? new.txt"),
        "{status}"
    );

    let candidate = ws.snapshot("child result").unwrap().unwrap();
    assert_eq!(candidate.branch, ws.name);
    assert_eq!(candidate.base, base);
    assert_eq!(candidate.head, fixture.head("by/feat"));
    assert_ne!(candidate.head, base);
    assert_eq!(candidate.stat, stat);
    let files = git(&ws.path, &["ls-tree", "--name-only", "HEAD"]);
    assert!(
        files.contains("new.txt") && !files.contains("debug.log"),
        "{files}"
    );
    assert_eq!(
        git(&ws.path, &["log", "-1", "--format=%s"]).trim(),
        "child result"
    );
    // The diff is still measured from base.
    assert_eq!(ws.diffstat().unwrap(), stat);
    // Snapshotting again without edits returns the same candidate.
    assert_eq!(ws.snapshot("again").unwrap().unwrap().head, candidate.head);

    let found = fixture.repo.workspace(&ws.name).unwrap().unwrap();
    assert_eq!(found, ws);
}

/// A same-size rewrite made in the same second as the checkout, with the
/// diff taken in a later second, still shows: git compares whole seconds
/// (racy git), so a stat-identical file is trusted as clean unless the
/// index it reads is no newer than the file. `diff` and `diffstat` work on a
/// copy of the index, which must keep the index's own time for that check.
#[test]
fn a_same_size_rewrite_in_the_checkout_second_is_diffed() {
    use std::os::unix::fs::MetadataExt;
    let fixture = Fixture::new();
    for attempt in 0.. {
        // Start just after a second begins, so the checkout and the rewrite
        // share it.
        let subsec_nanos = (branchyard_support::time::now_nanos() % 1_000_000_000) as u64;
        wait::settle(
            "to just after a second begins",
            Duration::from_nanos(1_000_000_000 - subsec_nanos),
        );
        let ws = fixture.workspace(&format!("racy{attempt}"));
        let file = ws.path.join("a.txt");
        fs::write(&file, "one\nTWO\nthree\n").unwrap();
        let index = git(
            &ws.path,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        );
        let index = fs::metadata(index.trim()).unwrap();
        let written = fs::metadata(&file).unwrap();
        if (written.mtime(), written.ctime()) != (index.mtime(), index.mtime()) {
            // Too slow to share the second; try again.
            assert!(attempt < 10, "never wrote within the checkout's second");
            continue;
        }
        // The diff runs in a later second than the checkout and the write.
        let next = Duration::from_secs(written.mtime() as u64 + 1);
        wait::until("the clock to reach the next second", || {
            Duration::from_millis(branchyard_support::time::now_ms()) >= next
        });
        let diff = ws.diff().unwrap();
        assert!(diff.contains("-two\n+TWO"), "{diff:?}");
        let stat = ws.diffstat().unwrap();
        assert_eq!(
            (stat.files_changed, stat.insertions, stat.deletions),
            (1, 1, 1)
        );
        let candidate = ws.snapshot("racy").unwrap().expect("a candidate");
        assert_eq!(candidate.stat, stat);
        return;
    }
}

#[test]
fn snapshot_without_changes_is_none() {
    let fixture = Fixture::new();
    let ws = fixture.workspace("idle");
    assert_eq!(ws.snapshot("nothing").unwrap(), None);
    fs::write(ws.path.join("only.log"), "ignored\n").unwrap();
    assert_eq!(ws.snapshot("nothing").unwrap(), None);
    assert_eq!(ws.diff().unwrap(), "");
    assert_eq!(fixture.head("by/idle"), ws.base);
}

#[test]
fn snapshot_rejects_head_off_branch() {
    let fixture = Fixture::new();
    let ws = fixture.workspace("wander");
    git(&ws.path, &["checkout", "-q", "--detach"]);
    assert!(matches!(
        ws.snapshot("x"),
        Err(GitError::NotOnBranch { .. })
    ));
}

#[test]
fn integrate_success_into_unchecked_out_target() {
    let fixture = Fixture::new();
    git(&fixture.root(), &["branch", "release"]);
    let expected = fixture.head("release");
    let candidate = fixture.candidate("feature", "feature.txt", "feature\n");

    let check = sh(
        "test -f feature.txt && echo check-ok",
        Duration::from_secs(30),
    );
    let done = fixture
        .repo
        .integrate(&candidate, "release", &expected, Some(&check))
        .unwrap();
    assert_eq!(done.target, "release");
    assert_eq!(done.previous, expected);
    assert_eq!(fixture.head("release"), done.merged);
    assert_eq!(fixture.head("release^1"), expected);
    assert_eq!(fixture.head("release^2"), candidate.head);
    assert!(done.check_output_tail.unwrap().contains("check-ok"));
    assert!(done.stale_checkouts.is_empty());
    // main is untouched and the user's tree still clean.
    assert_eq!(fixture.head("main"), expected);
    assert_eq!(git(&fixture.root(), &["status", "--porcelain"]), "");
    fixture.assert_no_integration_worktrees();
}

#[test]
fn integrate_updates_clean_checked_out_target() {
    let fixture = Fixture::new();
    let expected = fixture.head("main");
    let candidate = fixture.candidate("feature", "feature.txt", "feature\n");
    let done = fixture
        .repo
        .integrate(&candidate, "main", &expected, None)
        .unwrap();
    assert_eq!(done.check_output_tail, None);
    assert!(done.stale_checkouts.is_empty());
    assert_eq!(fixture.head("HEAD"), done.merged);
    assert_eq!(
        fs::read_to_string(fixture.root().join("feature.txt")).unwrap(),
        "feature\n"
    );
    assert_eq!(git(&fixture.root(), &["status", "--porcelain"]), "");
    fixture.assert_no_integration_worktrees();

    // The same candidate cannot be promoted twice.
    assert!(matches!(
        fixture
            .repo
            .integrate(&candidate, "main", &done.merged, None),
        Err(IntegrationError::AlreadyIntegrated)
    ));
    fixture.assert_no_integration_worktrees();
}

#[test]
fn check_failure_leaves_target_untouched() {
    let fixture = Fixture::new();
    let expected = fixture.head("main");
    let candidate = fixture.candidate("bad", "feature.txt", "feature\n");
    let check = sh("echo boom; echo err >&2; exit 3", Duration::from_secs(30));
    match fixture
        .repo
        .integrate(&candidate, "main", &expected, Some(&check))
    {
        Err(IntegrationError::CheckFailed {
            status,
            output_tail,
        }) => {
            assert_eq!(status.code(), Some(3));
            assert!(output_tail.contains("boom") && output_tail.contains("err"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fixture.head("main"), expected);
    assert!(!fixture.root().join("feature.txt").exists());
    fixture.assert_no_integration_worktrees();

    let missing = Check {
        argv: vec!["branchyard-no-such-program".into()],
        timeout: Duration::from_secs(5),
    };
    assert!(matches!(
        fixture
            .repo
            .integrate(&candidate, "main", &expected, Some(&missing)),
        Err(IntegrationError::CheckNotStarted(_))
    ));
    assert_eq!(fixture.head("main"), expected);
    fixture.assert_no_integration_worktrees();
}

#[test]
fn check_output_tail_is_bounded() {
    let fixture = Fixture::new();
    let expected = fixture.head("main");
    let candidate = fixture.candidate("loud", "feature.txt", "feature\n");
    let check = sh(
        "i=0; while [ $i -lt 2000 ]; do echo line-$i; i=$((i+1)); done; echo last; exit 1",
        Duration::from_secs(30),
    );
    match fixture
        .repo
        .integrate(&candidate, "main", &expected, Some(&check))
    {
        Err(IntegrationError::CheckFailed { output_tail, .. }) => {
            assert!(output_tail.len() <= branchyard_workspace::OUTPUT_TAIL_BYTES);
            assert!(output_tail.ends_with("last\n"));
            assert!(!output_tail.contains("line-0\n"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn check_timeout_kills_process_group() {
    let fixture = Fixture::new();
    let expected = fixture.head("main");
    let candidate = fixture.candidate("slow", "feature.txt", "feature\n");
    // The background sleep holds the output pipe; it must be killed too.
    let check = sh(
        "echo started; sleep 30 & sleep 30",
        Duration::from_millis(300),
    );
    let started = Instant::now();
    match fixture
        .repo
        .integrate(&candidate, "main", &expected, Some(&check))
    {
        Err(IntegrationError::CheckTimedOut {
            timeout,
            output_tail,
        }) => {
            assert_eq!(timeout, Duration::from_millis(300));
            assert!(output_tail.contains("started"));
        }
        other => panic!("{other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(fixture.head("main"), expected);
    fixture.assert_no_integration_worktrees();
}

/// A `by check` whose caller was killed kept its check running for its
/// whole timeout. A check nobody waits for any more is killed, its
/// process group with it, long before its timeout.
#[test]
fn an_abandoned_check_kills_its_process_group() {
    let fixture = Fixture::new();
    let main = fixture.head("main");
    let candidate = fixture.candidate("slow", "feature.txt", "feature\n");
    let pid = fixture.dir.join("abandoned-check.pid");
    let check = sh(
        &format!("sleep 30 & echo $! > '{}'; sleep 30", pid.display()),
        Duration::from_secs(60),
    );
    let name: BranchName = "slow".parse().unwrap();
    let merged = fixture
        .repo
        .merged_worktree(&name, &candidate.head, "main", &main)
        .unwrap();
    let started = Instant::now();
    let abandoned = || pid.exists() && started.elapsed() > Duration::from_millis(200);
    assert!(matches!(
        merged.check_until(&check, &abandoned),
        Err(IntegrationError::CheckAbandoned)
    ));
    assert!(started.elapsed() < Duration::from_secs(10));
    // The background sleep, in the check's group, was killed too.
    let background = fs::read_to_string(&pid).unwrap();
    let proc = PathBuf::from("/proc").join(background.trim());
    wait::until("the check's background sleep to be killed", || {
        !proc.exists() || fs::read_to_string(proc.join("stat")).is_ok_and(|s| s.contains(") Z "))
    });
    drop(merged);
    fs::remove_file(&pid).unwrap();
    fixture.assert_no_integration_worktrees();
}

#[test]
fn conflict_returns_files_and_aborts_cleanly() {
    let fixture = Fixture::new();
    let candidate = fixture.candidate("clash", "a.txt", "one\nchild\nthree\n");
    fs::write(fixture.root().join("other.txt"), "x\n").unwrap();
    let expected = fixture.commit_on_main("a.txt", "one\nparent\nthree\n");
    match fixture.repo.integrate(&candidate, "main", &expected, None) {
        Err(IntegrationError::Conflict { files }) => assert_eq!(files, ["a.txt"]),
        other => panic!("{other:?}"),
    }
    assert_eq!(fixture.head("main"), expected);
    assert_eq!(
        fs::read_to_string(fixture.root().join("a.txt")).unwrap(),
        "one\nparent\nthree\n"
    );
    // Untracked user files are not disturbed.
    assert!(fixture.root().join("other.txt").exists());
    assert_eq!(
        git(&fixture.root(), &["status", "--porcelain"]),
        "?? other.txt\n"
    );
    fixture.assert_no_integration_worktrees();
}

/// The battery's calc4 and recovery scenarios: siblings that share one
/// test suite cannot pass it one at a time, so each `integrate` alone is
/// refused; `integrate_many` merges them all in one worktree, checks the
/// result once and moves the target once.
#[test]
fn integrate_many_checks_the_combined_merge_once() {
    let fixture = Fixture::new();
    git(&fixture.root(), &["branch", "parent"]);
    let expected = fixture.head("parent");
    let a = fixture.candidate("a", "a-part.txt", "a\n");
    let b = fixture.candidate("b", "b-part.txt", "b\n");
    let counter = fixture.dir.join("checks-run");
    let suite = sh(
        &format!(
            "echo run >> '{}'; test -f a-part.txt && test -f b-part.txt && echo suite-ok",
            counter.display()
        ),
        Duration::from_secs(30),
    );
    for alone in [&a, &b] {
        assert!(matches!(
            fixture
                .repo
                .integrate(alone, "parent", &expected, Some(&suite)),
            Err(IntegrationError::CheckFailed { .. })
        ));
    }
    assert_eq!(fixture.head("parent"), expected);
    fs::remove_file(&counter).unwrap();

    let done = fixture
        .repo
        .integrate_many(&[a.clone(), b.clone()], "parent", &expected, &[suite])
        .unwrap();
    assert_eq!(done.previous, expected);
    assert_eq!(fixture.head("parent"), done.merged);
    assert_eq!(fs::read_to_string(&counter).unwrap(), "run\n", "one check");
    assert!(done.check_output_tails[0].contains("suite-ok"));
    // One merge commit per candidate, in order, on the target's line.
    assert_eq!(fixture.head("parent^1^1"), expected);
    assert_eq!(fixture.head("parent^1^2"), a.head);
    assert_eq!(fixture.head("parent^2"), b.head);
    let branches: Vec<&str> = done.candidates.iter().map(|c| c.branch.as_str()).collect();
    assert_eq!(branches, ["by/a", "by/b"]);
    assert!(done.candidates.iter().all(|c| c.merge.is_some()));
    // Merges stack: the second is made onto the first, not the old target.
    assert_eq!(done.candidates[0].onto, expected);
    assert_eq!(done.candidates[1].onto, fixture.head("parent^1"));
    assert_eq!(done.candidates[1].merge.as_ref(), Some(&done.merged));
    // The merge that brought each in.
    assert_eq!(
        fixture.repo.brought_in_by(&a.head, "parent").unwrap(),
        Some(fixture.head("parent^1"))
    );
    assert_eq!(
        fixture.repo.brought_in_by(&b.head, "parent").unwrap(),
        Some(done.merged.clone())
    );
    assert_eq!(fixture.repo.brought_in_by(&b.head, "main").unwrap(), None);
    fixture.assert_no_integration_worktrees();

    // Again: everything is contained, nothing moves, and it is not an error.
    let again = fixture
        .repo
        .integrate_many(&[a, b], "parent", &done.merged, &[])
        .unwrap();
    assert_eq!(again.merged, done.merged);
    assert!(again.candidates.iter().all(|c| c.merge.is_none()));
    fixture.assert_no_integration_worktrees();
}

#[test]
fn integrate_many_names_the_candidate_that_conflicts_and_moves_nothing() {
    let fixture = Fixture::new();
    let expected = fixture.head("main");
    let first = fixture.candidate("first", "first.txt", "1\n");
    let left = fixture.candidate("left", "a.txt", "one\nleft\nthree\n");
    let right = fixture.candidate("right", "a.txt", "one\nright\nthree\n");
    match fixture
        .repo
        .integrate_many(&[first, left, right], "main", &expected, &[])
    {
        Err(IntegrationError::ConflictWith {
            candidate,
            merged,
            files,
        }) => {
            assert_eq!(candidate, "by/right");
            assert_eq!(merged, ["by/first", "by/left"]);
            assert_eq!(files, ["a.txt"]);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fixture.head("main"), expected);
    assert_eq!(git(&fixture.root(), &["status", "--porcelain"]), "");
    fixture.assert_no_integration_worktrees();
}

/// The conflict3 and depth2 scenarios: a child that merged its sibling's
/// branch already carries it; the sibling is then contained, not refused.
#[test]
fn integrate_many_skips_a_candidate_an_earlier_one_brought_in() {
    let fixture = Fixture::new();
    let expected = fixture.head("main");
    let bounds = fixture.candidate("bounds", "bounds.txt", "b\n");
    let words = fixture.workspace("words");
    git(&words.path, &["merge", "-q", "--no-edit", "by/bounds"]);
    fs::write(words.path.join("words.txt"), "w\n").unwrap();
    let words = words.snapshot("words").unwrap().unwrap();
    let done = fixture
        .repo
        .integrate_many(&[words.clone(), bounds.clone()], "main", &expected, &[])
        .unwrap();
    assert!(done.candidates[0].merge.is_some());
    assert_eq!(done.candidates[1].merge, None, "brought in by words");
    assert_eq!(fixture.head("main^2"), words.head);
    assert_eq!(
        fixture.repo.brought_in_by(&bounds.head, "main").unwrap(),
        Some(done.merged)
    );
}

/// A check that fails on the combined merge refuses all of them: the
/// target stays, the checks after it never run, and the temporary
/// worktree is gone.
#[test]
fn integrate_many_refuses_a_failed_combined_check_and_cleans_up() {
    let fixture = Fixture::new();
    let expected = fixture.head("main");
    let a = fixture.candidate("a", "a-part.txt", "a\n");
    let b = fixture.candidate("b", "b-part.txt", "b\n");
    let ran = fixture.dir.join("second-ran");
    let passing = sh("true", Duration::from_secs(30));
    let failing = sh("echo suite-broken; exit 3", Duration::from_secs(30));
    let second = sh(
        &format!("touch '{}'", ran.display()),
        Duration::from_secs(30),
    );
    match fixture
        .repo
        .integrate_many(&[a, b], "main", &expected, &[passing, failing, second])
    {
        Err(IntegrationError::CheckStopped { index, error }) => {
            assert_eq!(index, 1, "the failed check is named by its place");
            match *error {
                IntegrationError::CheckFailed {
                    status,
                    output_tail,
                } => {
                    assert_eq!(status.code(), Some(3));
                    assert!(output_tail.contains("suite-broken"), "{output_tail}");
                }
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
    assert!(!ran.exists(), "a check after the failed one ran");
    assert_eq!(fixture.head("main"), expected);
    assert_eq!(git(&fixture.root(), &["status", "--porcelain"]), "");
    fixture.assert_no_integration_worktrees();
}

/// A check that runs past its timeout, or cannot start, refuses the
/// integration like a failed one.
#[test]
fn integrate_many_refuses_a_check_that_times_out_or_cannot_start() {
    let fixture = Fixture::new();
    let expected = fixture.head("main");
    let a = fixture.candidate("a", "a-part.txt", "a\n");
    let slow = sh("sleep 30", Duration::from_millis(200));
    assert!(matches!(
        fixture
            .repo
            .integrate_many(std::slice::from_ref(&a), "main", &expected, &[slow]),
        Err(IntegrationError::CheckStopped { index: 0, error })
            if matches!(*error, IntegrationError::CheckTimedOut { .. })
    ));
    let missing = Check {
        argv: vec!["/nonexistent/branchyard-check".into()],
        timeout: Duration::from_secs(30),
    };
    assert!(matches!(
        fixture
            .repo
            .integrate_many(&[a], "main", &expected, &[missing]),
        Err(IntegrationError::CheckStopped { index: 0, error })
            if matches!(*error, IntegrationError::CheckNotStarted(_))
    ));
    assert_eq!(fixture.head("main"), expected);
    fixture.assert_no_integration_worktrees();
}

/// Refusals before anything merges: a target that moved, an expected
/// commit that is not one, a forged candidate, a dirty checkout. None
/// moves the target or leaves a worktree.
#[test]
fn integrate_many_refuses_bad_inputs_before_merging() {
    let fixture = Fixture::new();
    let old = fixture.head("main");
    let a = fixture.candidate("a", "a-part.txt", "a\n");
    let moved = fixture.commit_on_main("b.txt", "b\n");
    match fixture
        .repo
        .integrate_many(std::slice::from_ref(&a), "main", &old, &[])
    {
        Err(IntegrationError::TargetMoved { expected, actual }) => {
            assert_eq!((expected, actual), (old.clone(), Some(moved.clone())));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        fixture.repo.integrate_many(
            std::slice::from_ref(&a),
            "main",
            &Commit("main".into()),
            &[]
        ),
        Err(IntegrationError::Git(GitError::InvalidRevision(_)))
    ));
    let mut forged = a.clone();
    forged.head = Commit("f".repeat(40));
    assert!(matches!(
        fixture
            .repo
            .integrate_many(&[a.clone(), forged], "main", &moved, &[]),
        Err(IntegrationError::InvalidCandidate(_))
    ));
    fs::write(fixture.root().join("a.txt"), "local edit\n").unwrap();
    match fixture.repo.integrate_many(&[a], "main", &moved, &[]) {
        Err(IntegrationError::DirtyTarget { worktree }) => assert_eq!(worktree, fixture.root()),
        other => panic!("{other:?}"),
    }
    assert_eq!(fixture.head("main"), moved);
    fixture.assert_no_integration_worktrees();
}

/// Nothing to merge is not an error: no candidates, or a candidate named
/// twice, whose second merge the first already made.
#[test]
fn integrate_many_takes_no_candidates_and_a_repeated_one() {
    let fixture = Fixture::new();
    let expected = fixture.head("main");
    let none = fixture
        .repo
        .integrate_many(&[], "main", &expected, &[])
        .unwrap();
    assert_eq!(none.merged, expected);
    assert!(none.candidates.is_empty() && none.check_output_tails.is_empty());

    let a = fixture.candidate("a", "a-part.txt", "a\n");
    let twice = fixture
        .repo
        .integrate_many(&[a.clone(), a.clone()], "main", &expected, &[])
        .unwrap();
    assert_eq!(fixture.head("main"), twice.merged);
    assert_eq!(twice.candidates[0].merge.as_ref(), Some(&twice.merged));
    assert_eq!(
        twice.candidates[1].merge, None,
        "the first merge brought it in"
    );
    assert_eq!(twice.candidates[1].onto, twice.merged);
    fixture.assert_no_integration_worktrees();
}

/// The target moved while the combined check ran: the compare-and-swap
/// loses, the concurrent commit stays, and nothing is left behind.
#[test]
fn integrate_many_loses_the_swap_to_a_concurrent_commit() {
    let fixture = Fixture::new();
    let expected = fixture.head("main");
    let a = fixture.candidate("a", "a-part.txt", "a\n");
    let b = fixture.candidate("b", "b-part.txt", "b\n");
    let check = sh(
        &format!(
            "git -C '{}' commit -q --allow-empty -m concurrent",
            fixture.root().display()
        ),
        Duration::from_secs(30),
    );
    match fixture
        .repo
        .integrate_many(&[a, b], "main", &expected, &[check])
    {
        Err(IntegrationError::TargetMoved {
            expected: e,
            actual,
        }) => {
            assert_eq!(e, expected);
            assert_eq!(actual, Some(fixture.head("main")));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fixture.head("main^"), expected);
    assert!(!fixture.root().join("a-part.txt").exists());
    fixture.assert_no_integration_worktrees();
}

/// A candidate the target already contains is recorded without a merge,
/// in its place among the others, and the next one merges onto the
/// target as it was.
#[test]
fn integrate_many_passes_over_a_candidate_the_target_contains() {
    let fixture = Fixture::new();
    let start = fixture.head("main");
    let old = fixture.candidate("old", "old.txt", "o\n");
    let first = fixture.repo.integrate(&old, "main", &start, None).unwrap();
    let new = fixture.candidate("new", "new.txt", "n\n");
    let done = fixture
        .repo
        .integrate_many(&[old.clone(), new], "main", &first.merged, &[])
        .unwrap();
    assert_eq!(done.candidates[0].merge, None);
    assert_eq!(done.candidates[0].onto, first.merged);
    assert_eq!(done.candidates[1].onto, first.merged);
    assert_eq!(done.candidates[1].merge.as_ref(), Some(&done.merged));
    assert_eq!(fixture.head("main^1"), first.merged);
    fixture.assert_no_integration_worktrees();
}

/// `check_commit` runs a check on one commit and says how it went:
/// passed, failed or timed out, leaving no worktree.
#[test]
fn check_commit_reports_each_outcome() {
    let fixture = Fixture::new();
    let head = fixture.head("main");
    let run = |script: &str, timeout| fixture.repo.check_commit(&head, &sh(script, timeout));
    assert!(matches!(
        run("test -f a.txt && echo here", Duration::from_secs(30)).unwrap(),
        CheckResult::Passed { output_tail } if output_tail.contains("here")
    ));
    assert!(matches!(
        run("echo broken; exit 1", Duration::from_secs(30)).unwrap(),
        CheckResult::Failed { output_tail } if output_tail.contains("broken")
    ));
    assert!(matches!(
        run("sleep 30", Duration::from_millis(200)).unwrap(),
        CheckResult::TimedOut { .. }
    ));
    let empty = Check {
        argv: Vec::new(),
        timeout: Duration::from_secs(30),
    };
    assert!(matches!(
        fixture.repo.check_commit(&head, &empty),
        Err(GitError::Io(e)) if e.to_string().contains("argv is empty")
    ));
    fixture.assert_no_integration_worktrees();
}

/// A candidate the target fast-forwarded to, then built on, is on the
/// target's first-parent line itself: it brought itself in, not the
/// unrelated commit after it.
#[test]
fn brought_in_by_a_fast_forward_is_the_commit_itself() {
    let fixture = Fixture::new();
    let candidate = fixture.candidate("ff", "ff.txt", "f\n");
    git(&fixture.root(), &["merge", "-q", "--ff-only", "by/ff"]);
    let later = fixture.commit_on_main("later.txt", "l\n");
    assert_ne!(later, candidate.head);
    assert_eq!(
        fixture.repo.brought_in_by(&candidate.head, "main").unwrap(),
        Some(candidate.head.clone())
    );
    // At the head itself, and after a merge, as before.
    assert_eq!(
        fixture.repo.brought_in_by(&later, "main").unwrap(),
        Some(later)
    );
}

#[test]
fn stale_candidate_after_target_moved() {
    let fixture = Fixture::new();
    let old = fixture.head("main");
    let candidate = fixture.candidate("stale", "feature.txt", "feature\n");
    let moved = fixture.commit_on_main("b.txt", "b\n");
    match fixture.repo.integrate(&candidate, "main", &old, None) {
        Err(IntegrationError::TargetMoved { expected, actual }) => {
            assert_eq!(expected, old);
            assert_eq!(actual, Some(moved.clone()));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fixture.head("main"), moved);
    fixture.assert_no_integration_worktrees();

    // Revalidating against the new target succeeds.
    let done = fixture
        .repo
        .integrate(&candidate, "main", &moved, None)
        .unwrap();
    assert_eq!(fixture.head("main^1"), moved);
    assert_eq!(fixture.head("main^2"), candidate.head);
    assert_eq!(done.previous, moved);
}

#[test]
fn target_moved_during_check_keeps_concurrent_commit() {
    let fixture = Fixture::new();
    let expected = fixture.head("main");
    let candidate = fixture.candidate("race", "feature.txt", "feature\n");
    let script = format!(
        "git -C '{}' commit -q --allow-empty -m concurrent",
        fixture.root().display()
    );
    let check = sh(&script, Duration::from_secs(30));
    match fixture
        .repo
        .integrate(&candidate, "main", &expected, Some(&check))
    {
        Err(IntegrationError::TargetMoved {
            expected: e,
            actual,
        }) => {
            assert_eq!(e, expected);
            assert_eq!(actual, Some(fixture.head("main")));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        git(&fixture.root(), &["log", "-1", "--format=%s", "main"]).trim(),
        "concurrent"
    );
    assert_eq!(fixture.head("main^"), expected);
    assert!(!fixture.root().join("feature.txt").exists());
    fixture.assert_no_integration_worktrees();
}

#[test]
fn dirty_checked_out_target_is_refused() {
    let fixture = Fixture::new();
    let expected = fixture.head("main");
    let candidate = fixture.candidate("dirty", "feature.txt", "feature\n");

    fs::write(fixture.root().join("a.txt"), "local edit\n").unwrap();
    match fixture.repo.integrate(&candidate, "main", &expected, None) {
        Err(IntegrationError::DirtyTarget { worktree }) => assert_eq!(worktree, fixture.root()),
        other => panic!("{other:?}"),
    }
    assert_eq!(fixture.head("main"), expected);
    assert_eq!(
        fs::read_to_string(fixture.root().join("a.txt")).unwrap(),
        "local edit\n"
    );
    git(&fixture.root(), &["checkout", "--", "a.txt"]);

    // An untracked file where the merge would add one is also refused.
    fs::write(fixture.root().join("feature.txt"), "mine\n").unwrap();
    assert!(matches!(
        fixture.repo.integrate(&candidate, "main", &expected, None),
        Err(IntegrationError::DirtyTarget { .. })
    ));
    assert_eq!(fixture.head("main"), expected);
    assert_eq!(
        fs::read_to_string(fixture.root().join("feature.txt")).unwrap(),
        "mine\n"
    );
    fixture.assert_no_integration_worktrees();
}

/// A file in the way of the merge is refused before the check runs, not
/// after it: the check may take many minutes.
#[test]
fn a_file_in_the_way_is_refused_before_the_check_runs() {
    let fixture = Fixture::new();
    let expected = fixture.head("main");
    let a = fixture.candidate("a", "feature.txt", "feature\n");
    let b = fixture.candidate("b", "b-part.txt", "b\n");
    fs::write(fixture.root().join("feature.txt"), "mine\n").unwrap();
    // Outside the repository, so the check's own mark is never in the way.
    let ran = fixture.dir.join("check-ran");
    let check = sh(
        &format!("touch '{}'", ran.display()),
        Duration::from_secs(30),
    );

    match fixture.repo.integrate(&a, "main", &expected, Some(&check)) {
        Err(IntegrationError::DirtyTarget { worktree }) => assert_eq!(worktree, fixture.root()),
        other => panic!("{other:?}"),
    }
    assert!(!ran.exists(), "the check ran before the refusal");
    match fixture
        .repo
        .integrate_many(&[b, a], "main", &expected, std::slice::from_ref(&check))
    {
        Err(IntegrationError::DirtyTarget { worktree }) => assert_eq!(worktree, fixture.root()),
        other => panic!("{other:?}"),
    }
    assert!(!ran.exists(), "the checks ran before the refusal");
    assert_eq!(fixture.head("main"), expected);
    assert_eq!(
        fs::read_to_string(fixture.root().join("feature.txt")).unwrap(),
        "mine\n"
    );
    fixture.assert_no_integration_worktrees();
}

#[test]
fn invalid_inputs_are_rejected() {
    let fixture = Fixture::new();
    let expected = fixture.head("main");
    let candidate = fixture.candidate("inputs", "feature.txt", "feature\n");
    assert!(matches!(
        fixture.repo.integrate(&candidate, "-main", &expected, None),
        Err(IntegrationError::Git(GitError::InvalidRef(_)))
    ));
    assert!(matches!(
        fixture.repo.integrate(&candidate, "a..b", &expected, None),
        Err(IntegrationError::Git(GitError::InvalidRef(_)))
    ));
    assert!(matches!(
        fixture
            .repo
            .integrate(&candidate, "absent", &expected, None),
        Err(IntegrationError::TargetMoved { actual: None, .. })
    ));
    let mut forged = candidate.clone();
    forged.head = Commit("f".repeat(40));
    assert!(matches!(
        fixture.repo.integrate(&forged, "main", &expected, None),
        Err(IntegrationError::InvalidCandidate(_))
    ));
    let mut unrelated = candidate.clone();
    unrelated.base = candidate.head.clone();
    unrelated.head = expected.clone();
    assert!(matches!(
        fixture.repo.integrate(&unrelated, "main", &expected, None),
        Err(IntegrationError::InvalidCandidate(_))
    ));
    assert_eq!(fixture.head("main"), expected);
    fixture.assert_no_integration_worktrees();
}

#[test]
fn remove_deletes_worktree_and_optionally_branch() {
    let fixture = Fixture::new();
    let keep = fixture.workspace("keep");
    let keep_path = keep.path.clone();
    fs::write(keep_path.join("scratch.txt"), "uncommitted\n").unwrap();
    keep.remove(false).unwrap();
    assert!(!keep_path.exists());
    fixture.head("by/keep");
    let name: BranchName = "keep".parse().unwrap();
    assert_eq!(fixture.repo.workspace(&name).unwrap(), None);

    let gone = fixture.workspace("gone");
    let gone_path = gone.path.clone();
    gone.remove(true).unwrap();
    assert!(!gone_path.exists());
    assert!(fixture.repo.resolve("by/gone").is_err());
    let config = git(&fixture.root(), &["config", "--list", "--local"]);
    assert!(!config.contains("by/gone"), "{config}");

    // A worktree whose directory vanished can still be removed.
    let vanished = fixture.workspace("vanished");
    fs::remove_dir_all(&vanished.path).unwrap();
    let listed = fixture
        .repo
        .workspace(&vanished.name)
        .unwrap()
        .expect("listed while prunable");
    listed.remove(true).unwrap();
    assert!(fixture.repo.workspaces().unwrap().is_empty());
}

#[test]
fn workspaces_lists_only_branchyard_branches() {
    let fixture = Fixture::new();
    let one = fixture.workspace("one");
    let two = fixture.workspace("two/nested");
    let other = fixture.dir.join("other");
    git(
        &fixture.root(),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "other",
            other.to_str().unwrap(),
        ],
    );
    let detached = fixture.dir.join("detached");
    git(
        &fixture.root(),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            detached.to_str().unwrap(),
        ],
    );
    let mut names: Vec<String> = fixture
        .repo
        .workspaces()
        .unwrap()
        .into_iter()
        .map(|w| w.name.to_string())
        .collect();
    names.sort();
    assert_eq!(names, ["one", "two/nested"]);
    assert_eq!(fixture.repo.workspace(&one.name).unwrap().unwrap(), one);
    assert_eq!(fixture.repo.workspace(&two.name).unwrap().unwrap(), two);
}

#[test]
fn fallback_identity_when_none_configured() {
    let fixture = Fixture::new();
    git(&fixture.root(), &["config", "--unset", "user.name"]);
    git(&fixture.root(), &["config", "--unset", "user.email"]);
    let expected = fixture.head("main");
    let candidate = fixture.candidate("anon", "feature.txt", "feature\n");
    let done = fixture
        .repo
        .integrate(&candidate, "main", &expected, None)
        .unwrap();
    for commit in [&candidate.head, &done.merged] {
        assert_eq!(
            git(
                &fixture.root(),
                &["log", "-1", "--format=%an <%ae>", commit.as_str()]
            )
            .trim(),
            "Branchyard <branchyard@localhost>"
        );
    }
}

#[test]
fn verify_runs_a_check_on_one_commit_without_merging() {
    let fixture = Fixture::new();
    let candidate = fixture.candidate("feature", "feature.txt", "feature\n");
    let main = fixture.head("main");
    let pass = sh(
        "test -f feature.txt && echo verified",
        Duration::from_secs(30),
    );
    let verified = fixture.repo.verify(&candidate.head, &pass).unwrap();
    assert!(verified.passed && !verified.timed_out);
    assert_eq!(verified.commit, candidate.head);
    assert!(verified.output_tail.contains("verified"));
    let fail = sh("echo nope; exit 3", Duration::from_secs(30));
    let verified = fixture.repo.verify(&candidate.head, &fail).unwrap();
    assert!(!verified.passed && !verified.timed_out);
    assert!(verified.output_tail.contains("nope"));
    let slow = sh("sleep 5", Duration::from_millis(200));
    assert!(
        fixture
            .repo
            .verify(&candidate.head, &slow)
            .unwrap()
            .timed_out
    );
    let missing = Check {
        argv: vec!["/nonexistent/check".into()],
        timeout: Duration::from_secs(5),
    };
    assert!(matches!(
        fixture.repo.verify(&candidate.head, &missing),
        Err(IntegrationError::CheckNotStarted(_))
    ));
    // Nothing moved and nothing was left behind.
    assert_eq!(fixture.head("main"), main);
    fixture.assert_no_integration_worktrees();
}

/// A branch's uncommitted work, checked as integrating it would check it:
/// merged onto the target's current commit, with nothing committed,
/// moved or left behind.
#[test]
fn check_merged_checks_uncommitted_work_on_the_target_without_moving_anything() {
    let fixture = Fixture::new();
    let ws = fixture.workspace("work");
    fs::write(ws.path.join("feature.txt"), "feature\n").unwrap();
    fs::write(ws.path.join("debug.log"), "ignored\n").unwrap();
    let head_before = fixture.head("by/work");
    let status_before = git(&ws.path, &["status", "--porcelain"]);
    let work = ws.working_commit("work in progress").unwrap();
    // Nothing in the worktree moved: its HEAD, index and files are as they were.
    assert_eq!(fixture.head("by/work"), head_before);
    assert_eq!(git(&ws.path, &["status", "--porcelain"]), status_before);
    let listed = git(&fixture.root(), &["ls-tree", "--name-only", work.as_str()]);
    assert!(listed.contains("feature.txt"), "{listed}");
    assert!(!listed.contains("debug.log"), "{listed}");

    // The target moved on since the branch began: the check sees both.
    let main = fixture.commit_on_main("main.txt", "main\n");
    let both = sh(
        "test -f feature.txt && test -f main.txt && echo both",
        Duration::from_secs(30),
    );
    let name: BranchName = "work".parse().unwrap();
    let verified = fixture
        .repo
        .check_merged(&name, &work, "main", &main, &both)
        .unwrap();
    assert!(verified.passed, "{}", verified.output_tail);
    assert!(verified.output_tail.contains("both"));
    assert_ne!(verified.commit, main);
    let fail = sh("echo nope; exit 2", Duration::from_secs(30));
    let verified = fixture
        .repo
        .check_merged(&name, &work, "main", &main, &fail)
        .unwrap();
    assert!(!verified.passed && !verified.timed_out);

    // Work the target already contains is checked on the target itself.
    let contained = fixture
        .repo
        .check_merged(&name, &head_before, "main", &main, &both)
        .unwrap();
    assert_eq!(contained.commit, main);
    assert!(!contained.passed);

    // A conflict with the target is said, and the check does not run.
    fs::write(ws.path.join("main.txt"), "other\n").unwrap();
    let clash = ws.working_commit("clash").unwrap();
    match fixture
        .repo
        .check_merged(&name, &clash, "main", &main, &both)
    {
        Err(IntegrationError::Conflict { files }) => assert_eq!(files, ["main.txt"]),
        other => panic!("{other:?}"),
    }
    assert_eq!(fixture.head("main"), main);
    assert_eq!(fixture.head("by/work"), head_before);
    fixture.assert_no_integration_worktrees();
}

#[test]
fn push_sends_exactly_the_commit_to_a_remote_branch() {
    let fixture = Fixture::new();
    let remote = fixture.dir.join("remote.git");
    git(
        &fixture.dir,
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );
    git(
        &fixture.root(),
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let first = fixture.candidate("feature", "feature.txt", "feature\n");
    fixture
        .repo
        .push("origin", &first.head, "by/feature", false)
        .unwrap();
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/by/feature"]).trim(),
        first.head.as_str()
    );
    // A commit that does not descend from what is there needs force.
    let other = fixture.candidate("other", "other.txt", "other\n");
    let refused = fixture
        .repo
        .push("origin", &other.head, "by/feature", false);
    assert!(
        matches!(refused, Err(GitError::Failed { .. })),
        "{refused:?}"
    );
    fixture
        .repo
        .push("origin", &other.head, "by/feature", true)
        .unwrap();
    assert_eq!(
        git(&remote, &["rev-parse", "refs/heads/by/feature"]).trim(),
        other.head.as_str()
    );
    for (remote_name, branch) in [("-u", "x"), ("origin", "-x"), ("origin", "a..b"), ("", "x")] {
        assert!(matches!(
            fixture.repo.push(remote_name, &other.head, branch, false),
            Err(GitError::InvalidRef(_))
        ));
    }
}

#[test]
fn worktreeinclude_names_only_ignored_literal_paths_that_exist() {
    use branchyard_workspace::include;
    let f = Fixture::new();
    let root = f.root();
    fs::write(
        root.join(".gitignore"),
        "*.log\n.env\nnode_modules/\n.vscode/\n",
    )
    .unwrap();
    git(&root, &["add", ".gitignore"]);
    git(&root, &["commit", "-q", "-m", "ignore"]);
    fs::write(root.join(".env"), "SECRET=1\n").unwrap();
    fs::create_dir_all(root.join(".vscode")).unwrap();
    fs::write(root.join(".vscode/settings.json"), "{}").unwrap();
    fs::create_dir_all(root.join("node_modules")).unwrap();
    fs::write(root.join("notes.txt"), "untracked, not ignored").unwrap();
    fs::write(
        root.join(include::WORKTREE_INCLUDE_FILE),
        "# carried\n.env\n.vscode/\nnode_modules\na.txt\nnotes.txt\n*.log\n../escape\nmissing\n",
    )
    .unwrap();
    let included = include::resolve(&root);
    assert_eq!(included.paths, [".env", ".vscode", "node_modules"]);
    let skipped = included.skipped.join("\n");
    for named in ["a.txt", "notes.txt", "*.log", "../escape"] {
        assert!(skipped.contains(named), "{named} not in {skipped}");
    }
    assert!(!skipped.contains("missing"), "{skipped}");
    // No file, nothing.
    fs::remove_file(root.join(include::WORKTREE_INCLUDE_FILE)).unwrap();
    assert_eq!(include::resolve(&root), include::Included::default());
}

/// A detached worktree at `rev`, as a warm pool's slot is.
fn detached(f: &Fixture, name: &str, rev: &str) -> PathBuf {
    let slot = f.dir.join(name);
    git(
        &f.root(),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            slot.to_str().unwrap(),
            rev,
        ],
    );
    slot
}

#[test]
fn a_detached_worktree_is_adopted_as_a_branch() {
    let f = Fixture::new();
    let base = f.head("main");
    // What setup produced (ignored) and a link beside the checkout.
    let slot = detached(&f, "slot", "main");
    fs::write(slot.join("build.log"), "built").unwrap();
    std::os::unix::fs::symlink(f.dir.join("scratch"), slot.join("shared")).unwrap();
    let name: BranchName = "warm".parse().unwrap();
    let ws = f
        .repo
        .adopt_worktree(&name, &base, &slot, &f.dir.join("warm"))
        .unwrap();
    assert!(!slot.exists());
    assert_eq!(ws.path, f.dir.join("warm"));
    assert_eq!(
        fs::read_to_string(ws.path.join("build.log")).unwrap(),
        "built"
    );
    assert!(fs::symlink_metadata(ws.path.join("shared"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        branchyard_workspace::git::current_branch(&ws.path).unwrap(),
        Some("by/warm".into())
    );
    // Listed like a created branch, with its base recorded.
    let listed = f.repo.workspace(&name).unwrap().unwrap();
    assert_eq!(listed.base, base);
    assert_eq!(listed.path, ws.path);

    // A slot behind the base is moved forward by the checkout.
    let older = f.head("main");
    let newer = f.commit_on_main("b.txt", "b\n");
    let slot = detached(&f, "slot2", older.as_str());
    let ws = f
        .repo
        .adopt_worktree(
            &"ahead".parse().unwrap(),
            &newer,
            &slot,
            &f.dir.join("ahead"),
        )
        .unwrap();
    assert_eq!(ws.base, newer);
    assert_eq!(fs::read_to_string(ws.path.join("b.txt")).unwrap(), "b\n");

    // An existing branch is refused before the slot is touched.
    let slot = detached(&f, "slot3", "main");
    let refused = f
        .repo
        .adopt_worktree(&name, &newer, &slot, &f.dir.join("again"));
    assert!(
        matches!(refused, Err(GitError::BranchExists(_))),
        "{refused:?}"
    );
    assert!(slot.is_dir() && !f.dir.join("again").exists());

    // A checkout that cannot happen (an untracked file in the way) removes
    // the moved worktree and leaves no branch.
    let slot = detached(&f, "slot4", older.as_str());
    fs::write(slot.join("b.txt"), "in the way").unwrap();
    let failed = f.repo.adopt_worktree(
        &"blocked".parse().unwrap(),
        &newer,
        &slot,
        &f.dir.join("blocked"),
    );
    assert!(failed.is_err(), "{failed:?}");
    assert!(!slot.exists() && !f.dir.join("blocked").exists());
    assert!(f
        .repo
        .workspace(&"blocked".parse().unwrap())
        .unwrap()
        .is_none());
    assert!(!git(&f.root(), &["branch", "--list", "by/blocked"]).contains("blocked"));
}
