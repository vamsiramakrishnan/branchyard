//! Per-turn checkpoints, rewind, fork at a checkpoint, `try` and compare,
//! against the fake ACP agent. Crashes are real: a child process (this test
//! binary, running the ignored `rewind_child` or `try_child` test) aborts at
//! an injected fault point between an intent and its effect.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use branchyard::{
    Activity, Branch, BranchStatus, CheckRun, Error, Policy, RecordedEvent, SessionContinuity, Yard,
};
use common::{git, text, Fixture};

fn checkpoint_turns(branch: &Branch) -> Vec<u32> {
    branch
        .checkpoints()
        .unwrap()
        .checkpoints
        .iter()
        .map(|e| e.checkpoint.turn)
        .collect()
}

fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn last_text(events: &[RecordedEvent]) -> String {
    let start = events
        .iter()
        .rposition(|e| matches!(e.activity, Activity::Prompt(_)))
        .unwrap();
    text(&events[start..])
}

fn prompts(events: &[RecordedEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Prompt(p) => Some(p.clone()),
            _ => None,
        })
        .collect()
}

fn rewound(events: &[RecordedEvent]) -> Vec<(u32, SessionContinuity)> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Rewound { to, session, .. } => Some((*to, session.clone())),
            _ => None,
        })
        .collect()
}

/// A branch `name` with one turn per prompt.
fn turns(f: &Fixture, name: &str, prompts: &[&str]) -> Branch {
    let mut branch = f
        .task(prompts[0])
        .name(name)
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    for prompt in &prompts[1..] {
        let options = branchyard::TaskOptions {
            policy: Policy::allow_all(),
            ..f.options()
        };
        branch = branch.send(prompt, options).unwrap();
    }
    branch
}

#[test]
fn each_turn_records_a_checkpoint_ref_that_survives_a_restart_and_goes_with_the_branch() {
    let f = Fixture::new();
    let branch = turns(
        &f,
        "cp",
        &["WRITE r.txt=1", "WRITE s.txt=2", "WHOAMI nothing changes"],
    );
    assert_eq!(branch.info().turns, 3);
    let list = branch.checkpoints().unwrap();
    assert_eq!(list.current, Some(3));
    let turns: Vec<u32> = list.checkpoints.iter().map(|e| e.checkpoint.turn).collect();
    assert_eq!(turns, [1, 2, 3]);
    for entry in &list.checkpoints {
        let c = &entry.checkpoint;
        assert!(entry.available, "{c:?}");
        assert!(
            c.git_ref.starts_with("refs/branchyard/cp/")
                && c.git_ref.ends_with(&format!("/turn-{}", c.turn)),
            "{}",
            c.git_ref
        );
        assert_eq!(f.git(&["rev-parse", &c.git_ref]).trim(), c.commit);
        assert_eq!(c.after, Some(c.turn - 1));
        assert!(c.session.is_some());
    }
    assert_eq!(list.checkpoints[0].prompt.as_deref(), Some("WRITE r.txt=1"));
    // Turn 1 has r.txt and not s.txt; a turn without changes keeps turn 2's commit.
    let one = &list.checkpoints[0].checkpoint.commit;
    assert!(f.git(&["ls-tree", "--name-only", one]).contains("r.txt"));
    assert!(!f.git(&["ls-tree", "--name-only", one]).contains("s.txt"));
    assert_eq!(
        list.checkpoints[1].checkpoint.commit,
        list.checkpoints[2].checkpoint.commit
    );
    let events = branch.events().unwrap();
    let recorded = events
        .iter()
        .filter(|e| matches!(e.activity, Activity::Checkpoint(_)))
        .count();
    assert_eq!(recorded, 3);

    // A new engine reads the same checkpoints.
    let again = Yard::open(&f.root).unwrap().branch("cp").unwrap();
    assert_eq!(again.checkpoints().unwrap(), list);

    f.yard.remove("cp").unwrap();
    assert_eq!(f.git(&["for-each-ref", "refs/branchyard/"]), "");
}

#[test]
fn a_fork_at_a_checkpoint_has_its_files_and_a_summarized_fresh_session() {
    let f = Fixture::new();
    let parent = turns(&f, "src", &["WRITE x.txt=1", "WRITE x.txt=2 WRITE y.txt=1"]);
    let options = branchyard::TaskOptions {
        policy: Policy::allow_all(),
        name: Some("at-one".into()),
        ..f.options()
    };
    let fork = parent.fork_at(1, "WHOAMI then continue", options).unwrap();
    let info = fork.info();
    assert_eq!(info.parent.as_deref(), Some("src"));
    let checkpoint_one = &parent.checkpoints().unwrap().checkpoints[0].checkpoint;
    assert_eq!(info.base, checkpoint_one.commit);
    assert_eq!(read(&info.worktree.join("x.txt")).as_deref(), Some("1\n"));
    assert!(!info.worktree.join("y.txt").exists());
    // The parent is untouched.
    assert_eq!(
        read(&parent.info().worktree.join("x.txt")).as_deref(),
        Some("2\n")
    );
    let events = fork.events().unwrap();
    let forked: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::ForkedAt {
                branch,
                turn,
                commit,
                session,
            } => Some((branch.clone(), *turn, commit.clone(), session.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(forked.len(), 1);
    let (from, turn, commit, session) = &forked[0];
    assert_eq!((from.as_str(), *turn), ("src", 1));
    assert_eq!(commit, &checkpoint_one.commit);
    match session {
        SessionContinuity::Summary { turns, reason } => {
            assert_eq!(turns, &[1]);
            // Its session went on to turn 2, and ACP cannot fork anyway;
            // the harness reason is checked first.
            assert!(reason.contains("cannot fork"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert!(events.iter().any(|e| matches!(
        &e.activity,
        Activity::Warning(w) if w.contains("forked from src at checkpoint 1") && w.contains("fresh session")
    )));
    let prompt = &prompts(&events)[0];
    assert!(
        prompt.contains("### Turn 1\nAsked: WRITE x.txt=1"),
        "{prompt}"
    );
    assert!(!prompt.contains("y.txt=1"), "{prompt}");
    assert!(
        prompt.ends_with("## Your task now\nWHOAMI then continue"),
        "{prompt}"
    );
    let reply = last_text(&events);
    // A fresh session: the fake agent opened it with session/new.
    assert!(reply.contains("resumed=false"), "{reply}");
    // The fork's own first turn is its checkpoint 1.
    assert_eq!(checkpoint_turns(&fork), [1]);
    // Checkpoint 0 is the parent's base.
    let from_base = parent
        .fork_at(
            0,
            "WHOAMI",
            branchyard::TaskOptions {
                name: Some("at-zero".into()),
                ..f.options()
            },
        )
        .unwrap();
    assert_eq!(from_base.info().base, parent.info().base);
    assert!(!from_base.info().worktree.join("x.txt").exists());
    assert!(matches!(
        parent.fork_at(9, "x", f.options()),
        Err(Error::State(message)) if message.contains("no checkpoint 9")
    ));
}

#[test]
fn a_rewind_goes_back_and_forward_resuming_the_session_only_where_it_ended() {
    let f = Fixture::new();
    let branch = turns(
        &f,
        "rw",
        &["WRITE r.txt=1", "WRITE r.txt=2", "WRITE r.txt=3"],
    );
    let session = branch.info().session.clone().unwrap();
    let worktree = branch.info().worktree.clone();
    let list = branch.checkpoints().unwrap();
    let commit = |n: usize| list.checkpoints[n - 1].checkpoint.commit.clone();

    let back = branch.rewind(1).unwrap();
    assert_eq!((back.from, back.to), (Some(3), 1));
    assert_eq!(back.commit, commit(1));
    assert!(matches!(&back.session, SessionContinuity::Summary { turns, .. } if turns == &[1]));
    assert_eq!(read(&worktree.join("r.txt")).as_deref(), Some("1\n"));
    assert_eq!(f.git(&["rev-parse", "by/rw"]).trim(), commit(1));
    let info = f.yard.branch("rw").unwrap().info().clone();
    assert_eq!(info.status, BranchStatus::Ready);
    assert_eq!(info.candidate.unwrap().commit, commit(1));
    assert_eq!(info.session, None);
    // Later checkpoints stay.
    assert_eq!(checkpoint_turns(&branch), [1, 2, 3]);

    // Forward again: turn 3 ended its session, so it resumes natively.
    let forward = branch.rewind(3).unwrap();
    assert_eq!(
        forward.session,
        SessionContinuity::Native {
            session: session.clone()
        }
    );
    assert_eq!(read(&worktree.join("r.txt")).as_deref(), Some("3\n"));
    let resumed = branch.send("WHOAMI", f.options()).unwrap();
    let events = resumed.events().unwrap();
    let reply = last_text(&events);
    assert!(
        reply.contains(&format!("session {session} resumed=true")),
        "{reply}"
    );
    let four = resumed.checkpoints().unwrap();
    assert_eq!(four.current, Some(4));
    assert_eq!(four.checkpoints[3].checkpoint.after, Some(3));

    // Back to 1: a fresh session whose prompt carries the summary.
    branch.rewind(1).unwrap();
    let fresh = branch.send("WHOAMI again", f.options()).unwrap();
    let events = fresh.events().unwrap();
    let reply = last_text(&events);
    assert!(reply.contains("resumed=false"), "{reply}");
    let prompt = prompts(&events).pop().unwrap();
    assert!(
        prompt.contains("### Turn 1\nAsked: WRITE r.txt=1"),
        "{prompt}"
    );
    assert!(!prompt.contains("r.txt=2"), "{prompt}");
    assert!(
        prompt.ends_with("## Your task now\nWHOAMI again"),
        "{prompt}"
    );
    assert!(events.iter().any(|e| matches!(
        &e.activity,
        Activity::Warning(w) if w.contains("fresh session") && w.contains("summary")
    )));
    let five = fresh.checkpoints().unwrap();
    assert_eq!(five.current, Some(5));
    assert_eq!(five.checkpoints[4].checkpoint.after, Some(1));
    // The next turn continues that fresh session, and the summary is gone.
    let next = fresh.send("WHOAMI", f.options()).unwrap();
    let events = next.events().unwrap();
    assert!(last_text(&events).contains("resumed=true"));
    assert_eq!(prompts(&events).pop().unwrap(), "WHOAMI");

    // To the base: a fresh session with only the prompt.
    let base = branch.rewind(0).unwrap();
    assert!(matches!(base.session, SessionContinuity::Fresh { .. }));
    assert!(!worktree.join("r.txt").exists());
    assert_eq!(
        f.yard.branch("rw").unwrap().info().status,
        BranchStatus::NoChanges
    );
    let after_base = branch.send("WHOAMI base", f.options()).unwrap();
    assert!(last_text(&after_base.events().unwrap()).contains("resumed=false"));
    let all = rewound(&after_base.events().unwrap());
    assert_eq!(
        all.iter().map(|(to, _)| *to).collect::<Vec<_>>(),
        [1, 3, 1, 0]
    );
}

#[test]
fn a_rewind_is_refused_while_a_turn_runs_and_over_uncommitted_changes() {
    let f = Fixture::new();
    let branch = turns(&f, "busy", &["WRITE r.txt=1", "WRITE r.txt=2"]);
    let worktree = branch.info().worktree.clone();
    std::thread::scope(|scope| {
        let running = scope.spawn(|| branch.send("HANG", f.options()));
        let deadline = Instant::now() + Duration::from_secs(60);
        while f.yard.branch("busy").unwrap().info().status != BranchStatus::Running {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(200));
        assert!(matches!(branch.rewind(1), Err(Error::Running(_))));
        assert_eq!(read(&worktree.join("r.txt")).as_deref(), Some("2\n"));
        f.yard.cancel("busy").unwrap();
        running.join().unwrap().unwrap();
    });
    fs::write(worktree.join("scratch.txt"), "mine").unwrap();
    match branch.rewind(1) {
        Err(Error::Denied(message)) => assert!(message.contains("scratch.txt"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert!(worktree.join("scratch.txt").exists());
    fs::remove_file(worktree.join("scratch.txt")).unwrap();
    branch.rewind(1).unwrap();
}

/// Rewind the branch at `BY_CHILD_BRANCH` in `BY_CHILD_ROOT` to
/// `BY_CHILD_TO`; `BRANCHYARD_FAULT` makes it abort part way.
#[test]
#[ignore = "the child process of the crash tests"]
fn rewind_child() {
    let (Some(root), Some(name), Some(to)) = (
        std::env::var_os("BY_CHILD_ROOT"),
        std::env::var("BY_CHILD_BRANCH").ok(),
        std::env::var("BY_CHILD_TO").ok(),
    ) else {
        return;
    };
    let yard = Yard::open(root).unwrap();
    let _ = yard.branch(&name).unwrap().rewind(to.parse().unwrap());
}

/// Try `BY_CHILD_BRANCH` in `BY_CHILD_ROOT`; `BRANCHYARD_FAULT` makes it
/// abort part way.
#[test]
#[ignore = "the child process of the crash tests"]
fn try_child() {
    let (Some(root), Some(name)) = (
        std::env::var_os("BY_CHILD_ROOT"),
        std::env::var("BY_CHILD_BRANCH").ok(),
    ) else {
        return;
    };
    let yard = Yard::open(root).unwrap();
    let _ = match name.as_str() {
        "--off" => yard.try_off(false).map(|_| ()),
        _ => yard.try_on(&name).map(|_| ()),
    };
}

fn crash(f: &Fixture, test: &str, env: &[(&str, &str)]) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([test, "--exact", "--ignored", "--test-threads=1"])
        .env("BY_CHILD_ROOT", &f.root)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (name, value) in env {
        command.env(name, value);
    }
    let status = command.status().unwrap();
    assert!(!status.success(), "the child was meant to abort");
}

fn recovered(events: &[RecordedEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Recovered { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_rewind_cut_short_is_finished_by_recovery() {
    for (fault, name) in [
        ("rewind-after-intent", "cut-intent"),
        ("rewind-after-reset", "cut-reset"),
    ] {
        let f = Fixture::new();
        let branch = turns(&f, name, &["WRITE r.txt=1", "WRITE r.txt=2 WRITE t.txt=2"]);
        let worktree = branch.info().worktree.clone();
        let one = branch.checkpoints().unwrap().checkpoints[0]
            .checkpoint
            .commit
            .clone();
        crash(
            &f,
            "rewind_child",
            &[
                ("BY_CHILD_BRANCH", name),
                ("BY_CHILD_TO", "1"),
                ("BRANCHYARD_FAULT", fault),
            ],
        );
        if fault == "rewind-after-intent" {
            // The engine stopped before touching the worktree.
            assert_eq!(read(&worktree.join("r.txt")).as_deref(), Some("2\n"));
        }
        let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
        let pending: i64 = db
            .query_row(
                "SELECT count(*) FROM steps WHERE step = 'rewind' AND outcome IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1, "{fault}");
        drop(db);

        let yard = Yard::open(&f.root).unwrap();
        let info = yard.branch(name).unwrap().info().clone();
        assert_eq!(info.status, BranchStatus::Ready, "{fault}");
        assert_eq!(info.candidate.unwrap().commit, one);
        assert_eq!(read(&worktree.join("r.txt")).as_deref(), Some("1\n"));
        assert!(!worktree.join("t.txt").exists());
        assert_eq!(git(&worktree, &["status", "--porcelain"]), "");
        let events = yard.branch(name).unwrap().events().unwrap();
        let reasons = recovered(&events);
        assert_eq!(reasons.len(), 1, "{fault}");
        assert!(
            reasons[0].contains("a rewind to checkpoint 1 had begun"),
            "{}",
            reasons[0]
        );
        assert_eq!(rewound(&events).len(), 1);
        assert_eq!(
            yard.branch(name).unwrap().checkpoints().unwrap().current,
            Some(1)
        );
    }
}

/// Every file under `root` but `.git` and `.branchyard`: bytes, and
/// permission bits or a link's target.
fn tree(root: &Path) -> BTreeMap<String, (Vec<u8>, u32)> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, (Vec<u8>, u32)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if rel == ".git" || rel == ".branchyard" {
                continue;
            }
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&path).unwrap();
                out.insert(
                    rel,
                    (target.into_os_string().into_encoded_bytes(), 0o120000),
                );
            } else if meta.is_dir() {
                out.insert(
                    format!("{rel}/"),
                    (Vec::new(), meta.permissions().mode() & 0o7777),
                );
                walk(root, &path, out);
            } else {
                out.insert(
                    rel,
                    (fs::read(&path).unwrap(), meta.permissions().mode() & 0o7777),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// A repository with a file to change, one to delete, an executable to
/// make plain, and a symbolic link; and a branch that does all of that and
/// adds a file in a new directory.
fn try_fixture() -> (Fixture, Branch) {
    let f = Fixture::new();
    fs::write(f.root.join("gone.txt"), "delete me\n").unwrap();
    fs::write(f.root.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(f.root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("a.txt", f.root.join("link")).unwrap();
    f.git(&["add", "."]);
    f.git(&["commit", "-q", "-m", "more files"]);
    // Permission bits git does not keep, which --off must keep anyway.
    fs::set_permissions(f.root.join("a.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    let branch = f
        .task(
            "SH rm gone.txt\nSH chmod -x run.sh\nSH ln -sf run.sh link\nSH mkdir -p new/deep && printf 'fresh\\n' > new/deep/n.txt\nSH printf 'changed\\n' > a.txt",
        )
        .name("tried")
        .base("HEAD")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    assert_eq!(
        branch.info().status,
        BranchStatus::Ready,
        "{:?}",
        branch.events()
    );
    (f, branch)
}

#[test]
fn try_applies_to_a_clean_checkout_and_off_restores_it_byte_for_byte() {
    let (f, _branch) = try_fixture();
    let before = tree(&f.root);
    assert_eq!(f.git(&["status", "--porcelain"]), "");

    let state = f.yard.try_on("tried").unwrap();
    assert_eq!(state.branch, "tried");
    let paths: Vec<&str> = state.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        ["a.txt", "gone.txt", "link", "new/deep/n.txt", "run.sh"]
    );
    assert_eq!(read(&f.root.join("a.txt")).as_deref(), Some("changed\n"));
    assert!(!f.root.join("gone.txt").exists());
    assert_eq!(
        read(&f.root.join("new/deep/n.txt")).as_deref(),
        Some("fresh\n")
    );
    assert_eq!(
        fs::metadata(f.root.join("run.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert_eq!(
        fs::read_link(f.root.join("link")).unwrap(),
        Path::new("run.sh")
    );
    assert!(
        fs::read_to_string(f.root.join(".branchyard/try/state.json"))
            .unwrap()
            .contains("\"phase\": \"applied\"")
    );
    assert_eq!(f.yard.try_status().unwrap().unwrap().branch, "tried");
    // Trying it again changes nothing.
    assert_eq!(f.yard.try_on("tried").unwrap(), state);

    let off = f.yard.try_off(false).unwrap().unwrap();
    assert_eq!(off.branch, "tried");
    assert_eq!(tree(&f.root), before);
    assert_eq!(f.git(&["status", "--porcelain"]), "");
    assert!(f.yard.try_status().unwrap().is_none());
    assert!(f.yard.try_off(false).unwrap().is_none());
    assert_eq!(f.git(&["for-each-ref", "refs/branchyard-try/"]), "");
}

#[test]
fn try_is_refused_over_uncommitted_work_and_a_conflict_applies_nothing() {
    let (f, _branch) = try_fixture();
    fs::write(f.root.join("mine.txt"), "work in progress\n").unwrap();
    match f.yard.try_on("tried") {
        Err(Error::Denied(message)) => {
            assert!(message.contains("uncommitted changes"), "{message}");
            assert!(message.contains("mine.txt"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(read(&f.root.join("a.txt")).as_deref(), Some("one\ntwo\n"));
    fs::remove_file(f.root.join("mine.txt")).unwrap();

    // The checkout moves on where the branch changed a.txt.
    fs::write(f.root.join("a.txt"), "moved on\n").unwrap();
    fs::write(f.root.join("n2.txt"), "x\n").unwrap();
    f.git(&["add", "."]);
    f.git(&["commit", "-q", "-m", "main moves"]);
    let before = tree(&f.root);
    match f.yard.try_on("tried") {
        Err(Error::Denied(message)) => {
            assert!(message.contains("do not apply"), "{message}");
            assert!(message.contains("nothing was changed"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(tree(&f.root), before);
    assert_eq!(f.git(&["status", "--porcelain"]), "");
    assert!(f.yard.try_status().unwrap().is_none());
}

#[test]
fn trying_another_branch_swaps_and_edits_since_are_kept_unless_forced() {
    let (f, _branch) = try_fixture();
    let before = tree(&f.root);
    f.task("WRITE other.txt=b")
        .name("other")
        .policy(Policy::allow_all())
        .run()
        .unwrap();
    f.yard.try_on("tried").unwrap();
    let swapped = f.yard.try_on("other").unwrap();
    assert_eq!(swapped.branch, "other");
    assert_eq!(read(&f.root.join("other.txt")).as_deref(), Some("b\n"));
    assert_eq!(read(&f.root.join("a.txt")).as_deref(), Some("one\ntwo\n"));
    assert!(f.root.join("gone.txt").exists());

    fs::write(f.root.join("other.txt"), "edited while trying\n").unwrap();
    match f.yard.try_off(false) {
        Err(Error::Denied(message)) => assert!(message.contains("other.txt"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        read(&f.root.join("other.txt")).as_deref(),
        Some("edited while trying\n")
    );
    f.yard.try_off(true).unwrap();
    assert_eq!(tree(&f.root), before);
}

#[test]
fn a_try_cut_short_is_rolled_back_exactly() {
    for fault in ["try-after-intent", "try-after-apply", "try-mid-restore"] {
        let (f, _branch) = try_fixture();
        let before = tree(&f.root);
        if fault == "try-mid-restore" {
            f.yard.try_on("tried").unwrap();
            crash(
                &f,
                "try_child",
                &[("BY_CHILD_BRANCH", "--off"), ("BRANCHYARD_FAULT", fault)],
            );
        } else {
            crash(
                &f,
                "try_child",
                &[("BY_CHILD_BRANCH", "tried"), ("BRANCHYARD_FAULT", fault)],
            );
        }
        let state = fs::read_to_string(f.root.join(".branchyard/try/state.json")).unwrap();
        assert!(
            state.contains("\"phase\": \"applying\"") || state.contains("\"phase\": \"restoring\""),
            "{fault}: {state}"
        );
        let note = f.yard.try_recover().unwrap().expect("a rollback");
        assert!(note.contains("restored"), "{note}");
        assert_eq!(tree(&f.root), before, "{fault}");
        assert_eq!(f.git(&["status", "--porcelain"]), "", "{fault}");
        assert!(f.yard.try_status().unwrap().is_none());
    }
}

#[test]
fn attempts_compare_side_by_side_with_checks_and_diffs() {
    let f = Fixture::new();
    let options = branchyard::TaskOptions {
        policy: Policy::allow_all(),
        check: Some(vec![
            "sh".into(),
            "-c".into(),
            "test -f shared.txt && test -f b.txt".into(),
        ]),
        ..f.options()
    };
    let branches = f
        .yard
        .task("WRITE shared.txt=1 WRITE a.txt=first")
        .options(options.clone())
        .name("pick")
        .run_on(&["gemini-cli", "qwen-code"])
        .unwrap();
    assert_eq!(branches.len(), 2);
    let names = f.yard.fan_branches("pick").unwrap();
    assert_eq!(names, ["pick-gemini-cli", "pick-qwen-code"]);
    f.task("WRITE b.txt=2 WRITE shared.txt=2")
        .options(options)
        .name("third")
        .run()
        .unwrap();

    let all = vec![names[0].clone(), "third".into()];
    let attempts = f.yard.compare(&all, false).unwrap();
    assert_eq!(attempts.len(), 2);
    let (one, three) = (&attempts[0], &attempts[1]);
    assert_eq!(one.turns, 1);
    assert_eq!(one.files, ["a.txt", "shared.txt"]);
    assert_eq!(one.unique_files, ["a.txt"]);
    assert_eq!(three.unique_files, ["b.txt"]);
    assert_eq!(
        (one.files_changed, one.insertions, one.deletions),
        (2, 2, 2)
    );
    assert!(one.duration_ms.is_some());
    assert_eq!(one.check, CheckRun::NotRun);

    let checked = f.yard.compare(&all, true).unwrap();
    assert!(
        matches!(checked[0].check, CheckRun::Failed { .. }),
        "{:?}",
        checked[0].check
    );
    assert_eq!(checked[1].check, CheckRun::Passed);
    // Checks run in private worktrees, which are gone.
    assert_eq!(f.git(&["worktree", "list"]).lines().count(), 4);

    let diff = f.yard.diff_between(&names[0], "third").unwrap();
    assert!(diff.contains("+++ b/b.txt"), "{diff}");
    assert!(diff.contains("--- a/a.txt"), "{diff}");
    assert!(matches!(
        f.yard.fan_branches("nope"),
        Err(Error::UnknownBranch(_))
    ));
}
