//! Hermetic tests against temporary repositories. Requires `git` and `sh`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;
use std::time::{Duration, Instant};

use branchyard_workspace::{
    BranchName, Candidate, Check, Commit, GitError, IntegrationError, Repository,
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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        std::thread::sleep(Duration::from_nanos(
            1_000_000_000 - u64::from(now.subsec_nanos()),
        ));
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
        while std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            < next
        {
            std::thread::sleep(Duration::from_millis(10));
        }
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
