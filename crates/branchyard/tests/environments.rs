//! Prepared environments on this host, against the fake ACP agent: setup
//! run once per environment key and restored for later branches, a new
//! key when a lockfile changes, shared directories linked and kept out of
//! candidates, a failed build falling back to the last good one (and
//! saying so), concurrent branches building once, `.worktreeinclude`,
//! `rebuild` and `prune`, and a half-built environment removed by
//! recovery after its engine was killed.

#![allow(clippy::let_underscore_must_use, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::fs;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use branchyard::{
    Activity, BranchStatus, EnvironmentOrigin, EnvironmentState, RecordedEvent, TaskOptions,
    WorkspaceReport, WorkspaceSpec, Yard,
};
use branchyard_testkit::wait;
use common::{fake_agent, text, Fixture};

fn reports(events: &[RecordedEvent]) -> Vec<WorkspaceReport> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Workspace(report) => Some(report.clone()),
            _ => None,
        })
        .filter(|r| r.phase == branchyard::WorkspacePhase::Setup)
        .collect()
}

/// A repository whose setup writes `deps/lib.txt` from its lockfile and
/// counts its runs in `setups.log` at the root.
fn fixture(lock: &str) -> Fixture {
    let f = Fixture::new();
    fs::write(f.root.join(".gitignore"), "deps/\n.env\nsetups.log\n").unwrap();
    fs::write(f.root.join("pnpm-lock.yaml"), lock).unwrap();
    f.git(&["add", "."]);
    f.git(&["commit", "-q", "-m", "lockfile"]);
    f
}

fn spec() -> WorkspaceSpec {
    WorkspaceSpec {
        setup: vec![
            "echo \"$BRANCHYARD_BRANCH\" >> \"$BRANCHYARD_ROOT/setups.log\"; \
             if grep -q broken pnpm-lock.yaml; then echo cannot install >&2; exit 7; fi; \
             mkdir -p deps && printf \"lib-%s\" \"$(cat pnpm-lock.yaml)\" > deps/lib.txt"
                .into(),
        ],
        prepare: true,
        ..WorkspaceSpec::default()
    }
}

/// The turn ran to its end: a `SH` prompt changes no file.
fn finished(branch: &branchyard::Branch) -> bool {
    matches!(
        branch.info().status,
        BranchStatus::Ready | BranchStatus::NoChanges
    )
}

fn with(f: &Fixture, spec: WorkspaceSpec) -> TaskOptions {
    TaskOptions {
        workspace: Some(spec),
        ..f.options()
    }
}

fn setups(f: &Fixture) -> usize {
    fs::read_to_string(f.root.join("setups.log"))
        .map(|t| t.lines().count())
        .unwrap_or(0)
}

fn commit_lock(f: &Fixture, lock: &str) {
    fs::write(f.root.join("pnpm-lock.yaml"), lock).unwrap();
    f.git(&["commit", "-q", "-am", "lock"]);
}

#[test]
fn setup_runs_once_per_key_and_later_branches_start_from_it() {
    let f = fixture("v1");
    let a = f
        .yard
        .task("SH cat deps/lib.txt")
        .options(with(&f, spec()))
        .name("a")
        .run()
        .unwrap();
    assert!(finished(&a), "{:?}", a.info().status);
    let events = a.events().unwrap();
    assert!(text(&events).contains("lib-v1"), "{}", text(&events));
    let built = reports(&events).pop().unwrap();
    assert!(built.ok, "{built:?}");
    let used = built.environment.unwrap();
    assert_eq!(used.origin, EnvironmentOrigin::Built);
    assert_eq!(setups(&f), 1);
    let envs = f.yard.environments();
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].state, EnvironmentState::Good);
    assert_eq!(envs[0].produced, ["deps"]);
    assert_eq!(envs[0].built_by, "a");
    assert_eq!(envs[0].inputs[0].path, "pnpm-lock.yaml");
    assert_eq!(f.yard.environment_key(&spec()), envs[0].key);

    // Same key: restored, setup does not run.
    let b = f
        .yard
        .task("SH cat deps/lib.txt")
        .options(with(&f, spec()))
        .name("b")
        .run()
        .unwrap();
    assert!(finished(&b), "{:?}", b.info().status);
    let events = b.events().unwrap();
    assert!(text(&events).contains("lib-v1"));
    assert_eq!(setups(&f), 1);
    let restored = reports(&events).pop().unwrap();
    assert!(restored.ok && restored.commands.is_empty(), "{restored:?}");
    let used = restored.environment.unwrap();
    assert_eq!(used.origin, EnvironmentOrigin::Restored);
    assert_eq!(used.key, envs[0].key);
    assert_eq!(used.built_by.as_deref(), Some("a"));
    // This host's filesystem decides: a clone where it can, else a copy.
    assert!(
        matches!(used.method.as_deref(), Some("clone" | "copy")),
        "{used:?}"
    );
    // Its copy is its own.
    fs::write(b.info().worktree.join("deps/lib.txt"), "changed").unwrap();
    assert_eq!(
        fs::read_to_string(a.info().worktree.join("deps/lib.txt")).unwrap(),
        "lib-v1"
    );
    assert!(f.yard.workspace("b").unwrap().ready);

    // A later send runs nothing.
    let sent = b.send("WRITE x.txt=1", f.options()).unwrap();
    assert!(finished(&sent), "{:?}", sent.info().status);
    assert_eq!(setups(&f), 1);

    // A new lockfile is a new key: setup runs again.
    commit_lock(&f, "v2");
    let c = f
        .yard
        .task("SH cat deps/lib.txt")
        .options(with(&f, spec()))
        .name("c")
        .run()
        .unwrap();
    assert!(text(&c.events().unwrap()).contains("lib-v2"));
    assert_eq!(setups(&f), 2);
    assert_eq!(f.yard.environments().len(), 2);
}

#[test]
fn shared_directories_are_links_kept_out_of_candidates_and_unlinked_on_removal() {
    let f = fixture("v1");
    // Not ignored: only the engine keeps the link out of the candidate.
    fs::write(f.root.join(".gitignore"), ".env\nsetups.log\n").unwrap();
    f.git(&["commit", "-q", "-am", "deps not ignored"]);
    let shared = WorkspaceSpec {
        share: vec!["deps".into()],
        ..spec()
    };
    let a = f
        .yard
        .task("WRITE a.txt=1")
        .options(with(&f, shared.clone()))
        .name("a")
        .run()
        .unwrap();
    let b = f
        .yard
        .task("WRITE b.txt=1")
        .options(with(&f, shared.clone()))
        .name("b")
        .run()
        .unwrap();
    assert_eq!(setups(&f), 1);
    let key = f.yard.environments()[0].key.clone();
    for branch in [&a, &b] {
        let deps = branch.info().worktree.join("deps");
        let target = fs::read_link(&deps).unwrap();
        assert!(
            target.ends_with(format!("environments/{key}/tree/deps")),
            "{target:?}"
        );
        let candidate = branch.info().candidate.clone().unwrap_or_else(|| {
            panic!(
                "{:?} {:?}",
                branch.info().status,
                text(&branch.events().unwrap())
            )
        });
        let files = common::git(
            &f.root,
            &["ls-tree", "-r", "--name-only", &candidate.commit],
        );
        assert!(!files.contains("deps"), "{files}");
    }
    let used = reports(&b.events().unwrap())
        .pop()
        .unwrap()
        .environment
        .unwrap();
    assert_eq!(used.shared, ["deps"]);
    assert_eq!(used.method.as_deref(), Some("link"));
    assert_eq!(
        fs::read_to_string(b.info().worktree.join("deps/lib.txt")).unwrap(),
        "lib-v1"
    );
    // One install serves both.
    fs::write(a.info().worktree.join("deps/new.txt"), "x").unwrap();
    assert!(b.info().worktree.join("deps/new.txt").is_file());

    // An environment a branch links into is never pruned.
    let pruned = f
        .yard
        .prune_environments(0, Duration::ZERO, std::slice::from_ref(&key));
    assert!(pruned.removed.is_empty(), "{pruned:?}");
    assert!(pruned.kept[0].1.contains("linked by a, b"), "{pruned:?}");

    // Removal takes the link, not what it points at.
    let worktree = b.info().worktree.clone();
    f.yard.remove("b").unwrap();
    assert!(!worktree.exists());
    assert!(f
        .root
        .join(format!(".branchyard/environments/{key}/tree/deps/lib.txt"))
        .is_file());
}

#[test]
fn a_restore_shares_what_the_current_spec_shares() {
    let f = fixture("v1");
    let shared = WorkspaceSpec {
        share: vec!["deps".into()],
        ..spec()
    };
    let a = f
        .yard
        .task("WRITE a.txt=1")
        .options(with(&f, shared))
        .name("a")
        .run()
        .unwrap();
    assert!(fs::symlink_metadata(a.info().worktree.join("deps"))
        .unwrap()
        .file_type()
        .is_symlink());
    // `share` is not in the key: the same environment, restored as the
    // configuration now says, a copy of its own and no link.
    let b = f
        .yard
        .task("WRITE b.txt=1")
        .options(with(&f, spec()))
        .name("b")
        .run()
        .unwrap();
    assert_eq!(setups(&f), 1);
    let used = reports(&b.events().unwrap())
        .pop()
        .unwrap()
        .environment
        .unwrap();
    assert_eq!(used.origin, EnvironmentOrigin::Restored);
    assert!(used.shared.is_empty(), "{used:?}");
    let deps = b.info().worktree.join("deps");
    assert!(fs::symlink_metadata(&deps).unwrap().is_dir());
    assert_eq!(fs::read_to_string(deps.join("lib.txt")).unwrap(), "lib-v1");
}

#[test]
fn a_failed_build_keeps_the_last_good_one_and_branches_say_so() {
    let f = fixture("v1");
    f.yard
        .task("WRITE a.txt=1")
        .options(with(&f, spec()))
        .name("good")
        .run()
        .unwrap();
    let good = f.yard.environments()[0].clone();

    commit_lock(&f, "broken");
    let c = f
        .yard
        .task("SH cat deps/lib.txt")
        .options(with(&f, spec()))
        .name("c")
        .run()
        .unwrap();
    // Its own build failed, and the last good one stood in.
    assert!(finished(&c), "{:?}", c.info().status);
    let events = c.events().unwrap();
    let setup = reports(&events);
    assert_eq!(setup.len(), 2, "{setup:?}");
    assert!(!setup[0].ok && setup[0].exit_code == Some(7));
    assert!(setup[0].output.contains("cannot install"));
    let used = setup[1].environment.clone().unwrap();
    assert_eq!(used.origin, EnvironmentOrigin::LastGood);
    assert_eq!(used.used.as_deref(), Some(good.key.as_str()));
    assert!(used.reason.unwrap().contains("its own build failed"));
    assert!(text(&events).contains("lib-v1"));
    assert_eq!(setups(&f), 2);
    let envs = f.yard.environments();
    let failed = envs
        .iter()
        .find(|e| e.state == EnvironmentState::Failed)
        .unwrap();
    assert_eq!(failed.built_by, "c");
    assert!(envs
        .iter()
        .any(|e| e.key == good.key && e.state == EnvironmentState::Good));

    // The next branch with the broken key does not try again.
    let d = f
        .yard
        .task("SH cat deps/lib.txt")
        .options(with(&f, spec()))
        .name("d")
        .run()
        .unwrap();
    assert!(finished(&d), "{:?}", d.info().status);
    assert_eq!(setups(&f), 2);
    let used = reports(&d.events().unwrap())
        .pop()
        .unwrap()
        .environment
        .unwrap();
    assert_eq!(used.origin, EnvironmentOrigin::LastGood);
    assert!(used.reason.unwrap().contains("by env rebuild"));

    // A rebuild that fails again leaves the good one alone.
    let rebuilt = f.yard.rebuild_environment(&spec()).unwrap();
    assert!(rebuilt.environment.is_none());
    assert!(!rebuilt.report.ok);
    assert!(f
        .yard
        .environments()
        .iter()
        .any(|e| e.key == good.key && e.state == EnvironmentState::Good));
    // Fixed: a rebuild builds it, and the failure is gone.
    commit_lock(&f, "v3");
    let rebuilt = f.yard.rebuild_environment(&spec()).unwrap();
    let info = rebuilt.environment.unwrap();
    assert_eq!(info.built_by, "by env rebuild");
    assert_eq!(info.key, f.yard.environment_key(&spec()));
    assert!(!f
        .root
        .join(".branchyard/environments")
        .read_dir()
        .unwrap()
        .any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".build-")));
}

#[test]
fn a_failed_build_with_no_good_one_fails_the_branch() {
    let f = fixture("broken");
    let branch = f
        .yard
        .task("WRITE a.txt=1")
        .options(with(&f, spec()))
        .name("first")
        .run()
        .unwrap();
    let BranchStatus::Failed { reason } = &branch.info().status else {
        panic!("{:?}", branch.info().status);
    };
    assert!(reason.contains("exited with status 7"), "{reason}");
    assert_eq!(f.yard.environments()[0].state, EnvironmentState::Failed);
}

#[test]
fn branches_of_one_fan_build_their_environment_once() {
    let f = fixture("v1");
    let branches = f
        .yard
        .task("SH cat deps/lib.txt")
        .options(with(&f, spec()))
        .name("fan")
        .run_on(&["gemini-cli", "qwen-code"])
        .unwrap();
    assert_eq!(setups(&f), 1);
    let mut origins = Vec::new();
    for branch in &branches {
        assert!(finished(branch), "{:?}", branch.info().status);
        let events = branch.events().unwrap();
        assert!(text(&events).contains("lib-v1"));
        origins.push(reports(&events).pop().unwrap().environment.unwrap().origin);
    }
    origins.sort_by_key(|o| format!("{o:?}"));
    assert_eq!(
        origins,
        [EnvironmentOrigin::Built, EnvironmentOrigin::Restored]
    );
}

#[test]
fn worktreeinclude_files_are_copied_like_copy_globs() {
    let f = fixture("v1");
    fs::write(f.root.join(".env"), "SECRET=1\n").unwrap();
    fs::write(f.root.join("notes.txt"), "not ignored\n").unwrap();
    fs::write(
        f.root.join(".worktreeinclude"),
        "# carried\n.env\nnotes.txt\n",
    )
    .unwrap();
    let branch = f
        .yard
        .task("SH cat .env")
        .options(with(&f, WorkspaceSpec::default()))
        .name("included")
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    assert!(text(&events).contains("SECRET=1"));
    let copy = events
        .iter()
        .find_map(|e| match &e.activity {
            Activity::Workspace(r) if r.phase == branchyard::WorkspacePhase::Copy => {
                Some(r.clone())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(copy.copied, [".env"]);
    assert!(
        copy.refused.iter().any(|r| r.contains("notes.txt")),
        "{copy:?}"
    );
    assert!(!branch.info().worktree.join("notes.txt").exists());
}

#[test]
fn pruning_keeps_the_newest_of_each_recipe() {
    let f = fixture("v1");
    for (n, lock) in ["v1", "v2", "v3"].iter().enumerate() {
        if n > 0 {
            commit_lock(&f, lock);
        }
        f.yard
            .task("WRITE a.txt=1")
            .options(with(&f, spec()))
            .name(format!("b{n}"))
            .run()
            .unwrap();
        // Distinct build times.
        wait::settle("distinct build times", Duration::from_millis(5));
    }
    assert_eq!(f.yard.environments().len(), 3);
    let newest = f.yard.environments()[0].key.clone();
    let pruned = f.yard.prune_environments(1, Duration::from_secs(3600), &[]);
    assert_eq!(pruned.removed.len(), 2, "{pruned:?}");
    let left = f.yard.environments();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].key, newest);
    // Even when it is old, the newest of a recipe is its last good build.
    let pruned = f.yard.prune_environments(1, Duration::ZERO, &[]);
    assert!(pruned.removed.is_empty(), "{pruned:?}");
}

/// Run a turn with `prepare` whose setup is in `BY_ENV_SETUP` on a branch
/// named `slow`, in the repository at `BY_ENV_ROOT`. Run only as the child
/// of the recovery test, which kills it.
#[test]
#[ignore = "the child process of the environment recovery test"]
fn environment_child() {
    let (Some(root), Some(setup), Some(agent)) = (
        std::env::var_os("BY_ENV_ROOT"),
        std::env::var("BY_ENV_SETUP").ok(),
        std::env::var("BY_ENV_AGENT").ok(),
    ) else {
        return;
    };
    let yard = Yard::open(root).unwrap();
    let _ = yard
        .task("WRITE done.txt=1")
        .options(TaskOptions {
            harness: Some("gemini-cli".into()),
            command: Some(vec![agent]),
            workspace: Some(WorkspaceSpec {
                setup: vec![setup],
                prepare: true,
                ..WorkspaceSpec::default()
            }),
            ..TaskOptions::default()
        })
        .name("slow")
        .run();
}

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_half_built_environment_is_removed_by_recovery() {
    let f = fixture("v1");
    let marker = f.dir.join("started");
    // The engine stops in setup; the journal is then given the pending
    // `environment` step an engine stopped mid-capture would have left.
    let setup = format!("touch {}; sleep 120", marker.display());
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "environment_child",
            "--exact",
            "--ignored",
            "--test-threads=1",
        ])
        .env("BY_ENV_ROOT", &f.root)
        .env("BY_ENV_AGENT", fake_agent())
        .env("BY_ENV_SETUP", &setup)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = Killed(child);
    wait::until("setup to start", || marker.exists());
    child.0.kill().unwrap();
    child.0.wait().unwrap();

    let staging = f
        .root
        .join(".branchyard/environments/.staging-0123456789abcdef01234567-1");
    fs::create_dir_all(staging.join("tree/deps")).unwrap();
    fs::write(staging.join("tree/deps/half.txt"), "half").unwrap();
    {
        let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
        let intent = serde_json::json!({
            "key": "0123456789abcdef01234567",
            "staging": staging,
            "produced": ["deps"],
        })
        .to_string();
        let inserted = db
            .execute(
                "INSERT INTO steps (incarnation, turn, step, branch, generation, intent, \
                 started_ms) SELECT incarnation, turn, 'environment', branch, generation, ?1, \
                 started_ms FROM steps WHERE branch = 'slow' AND step = 'setup'",
                [intent],
            )
            .unwrap();
        assert_eq!(inserted, 1);
    }

    let yard = Yard::open(&f.root).unwrap();
    let branch = yard.branch("slow").unwrap();
    assert_eq!(branch.info().status, BranchStatus::Interrupted);
    let reason = branch
        .events()
        .unwrap()
        .iter()
        .find_map(|e| match &e.activity {
            Activity::Recovered { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        reason.contains("removed the half-built environment 0123456789ab"),
        "{reason}"
    );
    assert!(!staging.exists());
    assert!(yard.environments().is_empty());
}
