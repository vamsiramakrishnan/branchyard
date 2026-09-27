//! Code into and out of an actor through the bridge: a worktree round trip
//! with every kind of change, the harness's commits coming back as commits
//! with its uncommitted changes on top, history rewritten below the base
//! refused, the host repository otherwise untouched, nothing from outside
//! the worktree coming back, and a host worktree that changed meanwhile
//! refused. Requires `git` and `sh`.

mod common;

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use branchyard_bridge::Endpoint;
use branchyard_sandbox::{ExecSpec, Process, SandboxProvider, SandboxSpec};
use branchyard_substrate::transfer::{self, Error};
use common::Cluster;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
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

/// A repository with a committed file of each kind, plus uncommitted
/// changes, an untracked file and an ignored one.
fn repository(dir: &Path) -> PathBuf {
    let root = dir.join("repo");
    fs::create_dir_all(root.join("src")).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Test"]);
    git(&root, &["config", "user.email", "test@localhost"]);
    git(
        &root,
        &[
            "config",
            "remote.origin.url",
            "https://secret@example.com/r.git",
        ],
    );
    fs::write(root.join("src/keep.txt"), "keep\n").unwrap();
    fs::write(root.join("src/edit.txt"), "before\n").unwrap();
    fs::write(root.join("gone.txt"), "delete me\n").unwrap();
    fs::write(root.join("tool.sh"), "#!/bin/sh\necho tool\n").unwrap();
    fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "initial"]);
    // Uncommitted state that must reach the actor.
    fs::write(root.join("src/keep.txt"), "keep, edited on the host\n").unwrap();
    fs::write(root.join("untracked.txt"), "untracked\n").unwrap();
    fs::create_dir_all(root.join("ignored")).unwrap();
    fs::write(root.join("ignored/cache"), "never sent\n").unwrap();
    root
}

fn guest_run(endpoint: &Endpoint, cwd: &Path, script: &str) -> String {
    let spec = ExecSpec {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        cwd: cwd.to_path_buf(),
        env: [
            ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ]
        .into(),
    };
    let mut process = endpoint.exec(&spec).unwrap();
    drop(process.take_stdin());
    let mut out = String::new();
    std::io::Read::read_to_string(&mut process.take_stdout().unwrap(), &mut out).unwrap();
    let mut err = String::new();
    std::io::Read::read_to_string(&mut process.take_stderr().unwrap(), &mut err).unwrap();
    let status = process.wait().unwrap();
    assert!(status.success(), "{script}: {status}: {err}");
    out
}

fn host_state(root: &Path) -> (String, String, String) {
    (
        git(root, &["for-each-ref"]),
        git(root, &["count-objects", "-v"]),
        git(root, &["config", "--local", "--list"]),
    )
}

#[test]
fn a_worktree_round_trips_every_kind_of_change() {
    let cluster = Cluster::start("round-trip");
    let provider = cluster.provider();
    provider.ensure(&SandboxSpec::new("actor")).unwrap();
    let endpoint = provider.endpoint("actor").unwrap();
    let root = repository(&cluster.scratch.0);
    let guest = cluster.scratch.path("guest/workspace");
    let before = host_state(&root);
    let head = git(&root, &["rev-parse", "HEAD"]);

    let pushed = transfer::push(&endpoint, &root, &guest).unwrap();
    assert_eq!(pushed.base, head.trim());
    // The actor sees the host's files, uncommitted state included, with
    // HEAD at the host's commit and none of the host's configuration.
    assert_eq!(
        fs::read_to_string(guest.join("src/keep.txt")).unwrap(),
        "keep, edited on the host\n"
    );
    assert!(guest.join("untracked.txt").is_file());
    assert!(!guest.join("ignored").exists());
    assert_eq!(git(&guest, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&guest, &["symbolic-ref", "--short", "HEAD"]), "main\n");
    let status = git(&guest, &["status", "--porcelain"]);
    assert!(status.contains(" M src/keep.txt"), "{status}");
    assert!(status.contains("?? untracked.txt"), "{status}");
    let config = git(&guest, &["config", "--local", "--list"]);
    assert!(!config.contains("secret"), "{config}");
    assert!(!config.contains("remote."), "{config}");

    // The harness: edits, adds, deletes, changes a mode, writes binary data
    // and links, commits, and writes outside the worktree.
    let outside = cluster.scratch.path("guest/outside.txt");
    guest_run(
        &endpoint,
        &guest,
        &format!(
            "printf 'after\\n' > src/edit.txt && rm gone.txt && chmod 755 src/keep.txt \
             && printf '\\000\\001\\377binary' > data.bin && mkdir -p new/dir \
             && printf 'new\\n' > new/dir/file.txt && ln -s ../src/keep.txt new/link \
             && ln -s /etc/passwd new/passwd-link && chmod 644 tool.sh \
             && printf 'escape\\n' > ../outside.txt && printf 'escape\\n' > {} \
             && mkdir -p ignored && printf 'guest cache\\n' > ignored/guest-cache \
             && git add -A && git commit -q -m 'harness commit' \
             && printf 'after commit\\n' > after-commit.txt",
            cluster.scratch.path("outside-absolute.txt").display()
        ),
    );

    let pulled = transfer::pull(&endpoint, &pushed, &root).unwrap();
    assert!(pulled.changed);
    let read = |p: &str| fs::read(root.join(p)).unwrap();
    assert_eq!(read("src/edit.txt"), b"after\n");
    assert_eq!(read("src/keep.txt"), b"keep, edited on the host\n");
    assert_eq!(read("data.bin"), b"\x00\x01\xffbinary");
    assert_eq!(read("new/dir/file.txt"), b"new\n");
    assert_eq!(read("after-commit.txt"), b"after commit\n");
    assert_eq!(read("untracked.txt"), b"untracked\n");
    assert!(!root.join("gone.txt").exists());
    let mode = |p: &str| fs::metadata(root.join(p)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode("src/keep.txt") & 0o111, 0o111);
    assert_eq!(mode("tool.sh") & 0o111, 0);
    assert_eq!(
        fs::read_link(root.join("new/link")).unwrap(),
        Path::new("../src/keep.txt")
    );
    // A link comes back as a link, never as what it points to.
    assert_eq!(
        fs::read_link(root.join("new/passwd-link")).unwrap(),
        Path::new("/etc/passwd")
    );
    // Nothing from outside the worktree, and nothing ignored, came back.
    assert!(!root.join("outside.txt").exists());
    assert!(!root.parent().unwrap().join("outside.txt").exists());
    assert!(!root.join("ignored/guest-cache").exists());
    assert_eq!(read("ignored/cache"), b"never sent\n");
    assert!(outside.exists(), "the harness did write outside");

    // The guest's commit is now the branch's HEAD, on top of the old one;
    // what it left uncommitted is a working-tree change. Other refs and
    // the configuration are untouched.
    assert_eq!(pulled.commits.len(), 1);
    assert_eq!(git(&root, &["rev-parse", "HEAD"]).trim(), pulled.commits[0]);
    assert_eq!(git(&root, &["rev-parse", "HEAD^"]), head);
    assert_eq!(
        git(&root, &["log", "-1", "--format=%s"]),
        "harness commit
"
    );
    assert_eq!(
        git(&root, &["symbolic-ref", "--short", "HEAD"]),
        "main
"
    );
    assert_eq!(
        git(&root, &["config", "--local", "--list"]),
        before.2,
        "configuration changed"
    );
    let status = git(&root, &["status", "--porcelain"]);
    assert_eq!(
        status,
        "?? after-commit.txt
"
    );
    assert!(git(&root, &["ls-files"]).contains("data.bin"));
    assert!(!git(&root, &["ls-files"]).contains("gone.txt"));
    provider.destroy("actor").unwrap();
}

#[test]
fn an_unchanged_worktree_comes_back_unchanged() {
    let cluster = Cluster::start("unchanged");
    let provider = cluster.provider();
    provider.ensure(&SandboxSpec::new("actor")).unwrap();
    let endpoint = provider.endpoint("actor").unwrap();
    let root = repository(&cluster.scratch.0);
    let guest = cluster.scratch.path("guest");
    let status = git(&root, &["status", "--porcelain"]);
    let pushed = transfer::push(&endpoint, &root, &guest).unwrap();
    let before = host_state(&root);
    let pulled = transfer::pull(&endpoint, &pushed, &root).unwrap();
    assert!(!pulled.changed);
    assert!(pulled.commits.is_empty());
    assert_eq!(git(&root, &["status", "--porcelain"]), status);
    // Without commits, the host repository is not written at all.
    assert_eq!(host_state(&root), before);

    // A second push into a directory that already holds the repository is
    // refused rather than mixed with it.
    assert!(matches!(
        transfer::push(&endpoint, &root, &guest),
        Err(Error::Guest(_))
    ));
    provider.destroy("actor").unwrap();
}

#[test]
fn a_host_worktree_that_changed_meanwhile_is_not_overwritten() {
    let cluster = Cluster::start("conflict");
    let provider = cluster.provider();
    provider.ensure(&SandboxSpec::new("actor")).unwrap();
    let endpoint = provider.endpoint("actor").unwrap();
    let root = repository(&cluster.scratch.0);
    let guest = cluster.scratch.path("guest");
    let pushed = transfer::push(&endpoint, &root, &guest).unwrap();
    guest_run(&endpoint, &guest, "printf 'guest\\n' > src/edit.txt");
    fs::write(root.join("src/edit.txt"), "host\n").unwrap();
    assert!(matches!(
        transfer::pull(&endpoint, &pushed, &root),
        Err(Error::WorktreeChanged(_))
    ));
    assert_eq!(
        fs::read_to_string(root.join("src/edit.txt")).unwrap(),
        "host\n"
    );
    provider.destroy("actor").unwrap();
}

#[test]
fn a_home_directory_is_replaced_by_the_actors() {
    let cluster = Cluster::start("home");
    let provider = cluster.provider();
    provider.ensure(&SandboxSpec::new("actor")).unwrap();
    let endpoint = provider.endpoint("actor").unwrap();
    let home = cluster.scratch.path("home");
    fs::create_dir_all(home.join(".config")).unwrap();
    fs::write(home.join(".config/settings"), "v1\n").unwrap();
    fs::write(home.join("stale"), "removed in the actor\n").unwrap();
    let guest = cluster.scratch.path("guest-home");
    transfer::push_tree(&endpoint, &home, &guest).unwrap();
    assert_eq!(
        fs::read_to_string(guest.join(".config/settings")).unwrap(),
        "v1\n"
    );
    fs::write(guest.join(".config/settings"), "v2\n").unwrap();
    fs::remove_file(guest.join("stale")).unwrap();
    symlink("/etc", guest.join("etc-link")).unwrap();
    transfer::pull_tree(&endpoint, &guest, &home).unwrap();
    assert_eq!(
        fs::read_to_string(home.join(".config/settings")).unwrap(),
        "v2\n"
    );
    assert!(!home.join("stale").exists());
    assert!(fs::symlink_metadata(home.join("etc-link"))
        .unwrap()
        .file_type()
        .is_symlink());
    // A missing home in the actor leaves the host's alone.
    transfer::pull_tree(&endpoint, &cluster.scratch.path("absent"), &home).unwrap();
    assert!(home.join(".config/settings").exists());
    let leftovers: Vec<_> = fs::read_dir(&cluster.scratch.0)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(".home."))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    provider.destroy("actor").unwrap();
}

#[test]
fn commits_come_back_as_commits_with_uncommitted_work_on_top() {
    let cluster = Cluster::start("commits");
    let provider = cluster.provider();
    provider.ensure(&SandboxSpec::new("actor")).unwrap();
    let endpoint = provider.endpoint("actor").unwrap();
    let root = repository(&cluster.scratch.0);
    // A clean worktree, as the engine leaves one between turns.
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "host work"]);
    let base = git(&root, &["rev-parse", "HEAD"]).trim().to_owned();
    let guest = cluster.scratch.path("guest");
    let pushed = transfer::push(&endpoint, &root, &guest).unwrap();

    // The harness: two commits of its own, by two authors, then a dirty
    // file and a staged one it never commits.
    guest_run(
        &endpoint,
        &guest,
        "printf 'one\\n' > first.txt && git add first.txt \
         && GIT_AUTHOR_NAME=Ada GIT_AUTHOR_EMAIL=ada@example.com \
            GIT_AUTHOR_DATE='2001-02-03T04:05:06Z' \
            git commit -q -m 'First harness commit' -m 'With a body.' \
         && printf 'two\\n' > second.txt && git rm -q gone.txt && git add second.txt \
         && GIT_AUTHOR_NAME=Grace GIT_AUTHOR_EMAIL=grace@example.com \
            git commit -q -m 'Second harness commit' \
         && printf 'dirty\\n' >> first.txt && printf 'staged\\n' > staged.txt \
         && git add staged.txt",
    );

    let pulled = transfer::pull(&endpoint, &pushed, &root).unwrap();
    assert!(pulled.changed);
    assert_eq!(pulled.commits.len(), 2);
    // Order, messages and authors are kept, on top of the base.
    let log = git(
        &root,
        &[
            "log",
            // Author time in Unix seconds: git versions differ in how they
            // print a UTC offset in ISO dates (`+00:00` or `Z`).
            "--format=%H|%an <%ae>|%at|%s|%b",
            &format!("{base}..HEAD"),
        ],
    );
    let lines: Vec<&str> = log.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 2, "{log}");
    assert!(
        lines[0].starts_with(&format!("{}|Grace <grace@example.com>|", pulled.commits[1])),
        "{log}"
    );
    assert!(lines[0].ends_with("|Second harness commit|"), "{log}");
    assert!(
        lines[1].starts_with(&format!(
            "{}|Ada <ada@example.com>|981173106|First harness commit|With a body.",
            pulled.commits[0]
        )),
        "{log}"
    );
    assert_eq!(git(&root, &["rev-parse", "HEAD~2"]).trim(), base);
    assert_eq!(git(&root, &["symbolic-ref", "--short", "HEAD"]), "main\n");
    // Uncommitted work is a working-tree change on top of the commits.
    let status = git(&root, &["status", "--porcelain"]);
    assert_eq!(status, " M first.txt\n?? staged.txt\n", "{status}");
    assert_eq!(
        fs::read_to_string(root.join("first.txt")).unwrap(),
        "one\ndirty\n"
    );
    assert!(!root.join("gone.txt").exists());

    // The engine's snapshot then records a candidate on top of them.
    git(&root, &["add", "--all", "--", "."]);
    git(&root, &["commit", "-q", "-m", "snapshot"]);
    git(&root, &["merge-base", "--is-ancestor", &base, "HEAD"]);
    assert_eq!(
        git(&root, &["rev-list", "--count", &format!("{base}..HEAD")]),
        "3\n"
    );
    let diff = git(&root, &["diff", "--name-status", &base, "HEAD"]);
    assert_eq!(
        diff,
        "A\tfirst.txt\nD\tgone.txt\nA\tsecond.txt\nA\tstaged.txt\n"
    );
    git(&root, &["fsck", "--no-progress"]);
    provider.destroy("actor").unwrap();
}

#[test]
fn history_rewritten_below_the_base_is_refused() {
    let cluster = Cluster::start("rewrite");
    let provider = cluster.provider();
    provider.ensure(&SandboxSpec::new("actor")).unwrap();
    let endpoint = provider.endpoint("actor").unwrap();
    let root = repository(&cluster.scratch.0);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "second"]);
    let base = git(&root, &["rev-parse", "HEAD"]);
    let status = git(&root, &["status", "--porcelain"]);

    for (name, script) in [
        // Back below the base, then a new commit there.
        (
            "reset",
            "git reset -q --hard HEAD~1 && printf 'x\\n' > x.txt && git add x.txt \
             && git commit -q -m 'rewritten'",
        ),
        // The base amended.
        ("amend", "git commit -q --amend -m 'amended'"),
        // An unrelated history.
        (
            "orphan",
            "git checkout -q --orphan fresh && git commit -q -m 'orphan'",
        ),
    ] {
        let guest = cluster.scratch.path(&format!("guest-{name}"));
        let pushed = transfer::push(&endpoint, &root, &guest).unwrap();
        guest_run(&endpoint, &guest, script);
        match transfer::pull(&endpoint, &pushed, &root) {
            Err(Error::Rewritten(why)) => assert!(why.contains("does not descend"), "{why}"),
            other => panic!("{name}: expected a refusal, got {other:?}"),
        }
        // Nothing was applied.
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), base, "{name}");
        assert_eq!(git(&root, &["status", "--porcelain"]), status, "{name}");
        assert!(!root.join("x.txt").exists(), "{name}");
    }
    provider.destroy("actor").unwrap();
}

#[test]
fn commits_are_not_applied_over_a_host_branch_that_moved() {
    let cluster = Cluster::start("moved");
    let provider = cluster.provider();
    provider.ensure(&SandboxSpec::new("actor")).unwrap();
    let endpoint = provider.endpoint("actor").unwrap();
    let root = repository(&cluster.scratch.0);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "second"]);
    let guest = cluster.scratch.path("guest");
    let pushed = transfer::push(&endpoint, &root, &guest).unwrap();
    guest_run(
        &endpoint,
        &guest,
        "printf 'g\\n' > g.txt && git add g.txt && git commit -q -m guest",
    );
    git(
        &root,
        &["commit", "-q", "--allow-empty", "-m", "host moved on"],
    );
    let head = git(&root, &["rev-parse", "HEAD"]);
    assert!(matches!(
        transfer::pull(&endpoint, &pushed, &root),
        Err(Error::WorktreeChanged(_))
    ));
    assert_eq!(git(&root, &["rev-parse", "HEAD"]), head);
    assert!(!root.join("g.txt").exists());
    provider.destroy("actor").unwrap();
}
