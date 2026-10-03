//! Warm pools on this host, against the fake ACP agent: a fill makes ready
//! worktrees with the prepared environment in place, a branch takes one
//! and runs no setup, branches asking at once never share one, a keeper
//! refills after a claim, slots go stale when setup changes, the base
//! moves past policy or an input changes, or they age out, and a filler
//! killed midway leaves nothing behind once recovery runs.

mod common;

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use branchyard::{
    Activity, BranchStatus, EnvironmentOrigin, PoolFill, PoolSlotState, PoolSpec, PoolUse,
    RecordedEvent, TaskOptions, WorkspaceReport, WorkspaceSpec, Yard,
};
use common::{git, text, Fixture};

fn setup_report(events: &[RecordedEvent]) -> WorkspaceReport {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Workspace(report) => Some(report.clone()),
            _ => None,
        })
        .rfind(|r| r.phase == branchyard::WorkspacePhase::Setup)
        .expect("a setup report")
}

fn pool_use(branch: &branchyard::Branch) -> PoolUse {
    *setup_report(&branch.events().unwrap())
        .pool
        .expect("the report says how the pool was used")
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

fn spec(size: u32) -> WorkspaceSpec {
    WorkspaceSpec {
        setup: vec![
            "echo \"$BRANCHYARD_BRANCH\" >> \"$BRANCHYARD_ROOT/setups.log\"; \
             mkdir -p deps && printf \"lib-%s\" \"$(cat pnpm-lock.yaml)\" > deps/lib.txt"
                .into(),
        ],
        prepare: true,
        pool: Some(PoolSpec {
            size,
            ..PoolSpec::default()
        }),
        ..WorkspaceSpec::default()
    }
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

fn finished(branch: &branchyard::Branch) -> bool {
    matches!(
        branch.info().status,
        BranchStatus::Ready | BranchStatus::NoChanges
    )
}

fn ready(f: &Fixture, spec: &WorkspaceSpec) -> Vec<String> {
    f.yard
        .pool_status(spec)
        .unwrap()
        .slots
        .into_iter()
        .filter(|s| s.state == PoolSlotState::Ready)
        .map(|s| s.id)
        .collect()
}

fn head(dir: &Path) -> String {
    git(dir, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// Worktrees git lists under the pool's directory.
fn pool_worktrees(f: &Fixture) -> Vec<String> {
    git(&f.root, &["worktree", "list", "--porcelain"])
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .filter(|p| p.contains("/.branchyard/pool/"))
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_fill_makes_ready_slots_with_the_environment_in_place() {
    let f = fixture("v1");
    let spec = spec(2);
    let filled = f.yard.fill_pool(&spec).unwrap();
    assert_eq!(filled.made.len(), 2, "{filled:?}");
    assert_eq!(filled.ready, 2);
    assert!(filled.error.is_none() && filled.skipped.is_none());
    // Setup ran once, in the first slot, and built the key's environment;
    // the second slot restored it.
    assert_eq!(setups(&f), 1);
    let envs = f.yard.environments();
    assert_eq!(envs.len(), 1);
    assert_eq!(envs[0].built_by, "pool");
    let main = head(&f.root);
    for slot in &filled.made {
        assert_eq!(slot.state, PoolSlotState::Ready);
        assert_eq!(slot.environment.as_deref(), Some(envs[0].key.as_str()));
        assert!(slot.fill_ms.is_some());
        assert_eq!(head(&slot.path), main);
        assert_eq!(
            fs::read_to_string(slot.path.join("deps/lib.txt")).unwrap(),
            "lib-v1"
        );
        // Detached, clean.
        assert!(git(&slot.path, &["status", "--porcelain"]).is_empty());
        assert!(git(&slot.path, &["branch", "--show-current"])
            .trim()
            .is_empty());
    }
    assert_eq!(ready(&f, &spec).len(), 2);
    // Full: a second fill makes nothing.
    let again = f.yard.fill_pool(&spec).unwrap();
    assert!(
        again.made.is_empty() && again.discarded.is_empty(),
        "{again:?}"
    );
    assert_eq!(again.ready, 2);
    // Drained: nothing left on disk or in git.
    let drained = f.yard.drain_pool().unwrap();
    assert_eq!(drained.removed.len(), 2, "{drained:?}");
    assert!(f.yard.pool_slots().unwrap().is_empty());
    assert!(pool_worktrees(&f).is_empty());
}

#[test]
fn a_branch_takes_a_ready_slot_and_runs_no_setup() {
    let f = fixture("v1");
    let spec = spec(1);
    let filled = f.yard.fill_pool(&spec).unwrap();
    let slot = filled.made[0].clone();
    assert_eq!(setups(&f), 1);

    let a = f
        .yard
        .task("SH cat deps/lib.txt")
        .options(with(&f, spec.clone()))
        .name("a")
        .run()
        .unwrap();
    assert!(finished(&a), "{:?}", a.info().status);
    assert!(text(&a.events().unwrap()).contains("lib-v1"));
    // Its worktree is the slot's, moved to the branch's path.
    let used = pool_use(&a);
    assert_eq!(used.slot.as_deref(), Some(slot.id.as_str()), "{used:?}");
    assert!(used.reason.is_none(), "{used:?}");
    assert!(used.requested_ms > 0);
    assert!(!slot.path.exists());
    let worktree = a.info().worktree.clone();
    assert!(worktree.ends_with(".branchyard/worktrees/a"));
    assert_eq!(git(&worktree, &["branch", "--show-current"]).trim(), "by/a");
    assert_eq!(head(&worktree), head(&f.root));
    // Its setup restored nothing: the environment was already there.
    let report = setup_report(&a.events().unwrap());
    assert!(report.ok && report.commands.is_empty(), "{report:?}");
    let env = report.environment.unwrap();
    assert_eq!(env.origin, EnvironmentOrigin::Restored);
    assert_eq!(env.key, slot.environment.clone().unwrap());
    assert_eq!(env.method, slot.method);
    assert_eq!(setups(&f), 1);
    assert!(f.yard.workspace("a").unwrap().ready);
    assert!(f.yard.pool_slots().unwrap().is_empty());

    // The pool is empty and nothing refills it here: the next branch
    // misses, says why, and restores the environment itself.
    let b = f
        .yard
        .task("SH cat deps/lib.txt")
        .options(with(&f, spec.clone()))
        .name("b")
        .run()
        .unwrap();
    assert!(finished(&b), "{:?}", b.info().status);
    assert!(text(&b.events().unwrap()).contains("lib-v1"));
    let used = pool_use(&b);
    assert_eq!(used.slot, None);
    assert_eq!(used.reason.as_deref(), Some("no ready slot"));
    assert_eq!(setups(&f), 1);

    // A branch without a pool says nothing about one.
    let mut plain = spec.clone();
    plain.pool = None;
    let c = f
        .yard
        .task("SH true")
        .options(with(&f, plain))
        .name("c")
        .run()
        .unwrap();
    assert!(setup_report(&c.events().unwrap()).pool.is_none());

    // Removing a branch that came from a slot removes its worktree.
    f.yard.remove("a").unwrap();
    assert!(!worktree.exists());
}

#[test]
fn shared_directories_in_a_slot_stay_links_for_the_branch() {
    let f = fixture("v1");
    let mut spec = spec(1);
    spec.share = vec!["deps".into()];
    let filled = f.yard.fill_pool(&spec).unwrap();
    let slot = &filled.made[0];
    assert!(fs::symlink_metadata(slot.path.join("deps"))
        .unwrap()
        .file_type()
        .is_symlink());
    // The environment a slot links into is not pruned.
    let key = slot.environment.clone().unwrap();
    let pruned = f
        .yard
        .prune_environments(0, Duration::ZERO, std::slice::from_ref(&key));
    assert!(pruned.removed.is_empty(), "{pruned:?}");
    assert!(pruned.kept[0].1.contains("pool slot"), "{pruned:?}");

    let a = f
        .yard
        .task("SH cat deps/lib.txt")
        .options(with(&f, spec.clone()))
        .name("a")
        .run()
        .unwrap();
    assert!(finished(&a), "{:?}", a.info().status);
    assert!(pool_use(&a).slot.is_some());
    let worktree = a.info().worktree.clone();
    assert!(fs::symlink_metadata(worktree.join("deps"))
        .unwrap()
        .file_type()
        .is_symlink());
    let env = setup_report(&a.events().unwrap()).environment.unwrap();
    assert_eq!(env.shared, ["deps"]);
    // Never in a candidate, though no ignore rule is needed for a link.
    let changed = a.diff().unwrap();
    assert!(!changed.contains("deps"), "{changed}");
}

#[test]
fn branches_asking_at_once_never_share_a_slot() {
    let f = fixture("v1");
    let spec = spec(3);
    let filled = f.yard.fill_pool(&spec).unwrap();
    assert_eq!(filled.ready, 3);
    let names: Vec<String> = (0..6).map(|i| format!("racer-{i}")).collect();
    let uses: Vec<PoolUse> = std::thread::scope(|scope| {
        let handles: Vec<_> = names
            .iter()
            .map(|name| {
                let yard = Yard::open(&f.root).unwrap();
                let options = with(&f, spec.clone());
                scope.spawn(move || {
                    let branch = yard
                        .task("SH cat deps/lib.txt")
                        .options(options)
                        .name(name)
                        .run()
                        .unwrap();
                    assert!(finished(&branch), "{:?}", branch.info().status);
                    assert!(text(&branch.events().unwrap()).contains("lib-v1"));
                    pool_use(&branch)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut taken: Vec<String> = uses.iter().filter_map(|u| u.slot.clone()).collect();
    assert_eq!(taken.len(), 3, "{uses:?}");
    taken.sort();
    taken.dedup();
    assert_eq!(taken.len(), 3, "a slot went to two branches: {uses:?}");
    let mut made: Vec<String> = filled.made.iter().map(|s| s.id.clone()).collect();
    made.sort();
    assert_eq!(taken, made);
    assert_eq!(uses.iter().filter(|u| u.slot.is_none()).count(), 3);
    assert!(f.yard.pool_slots().unwrap().is_empty());
    // Each branch has its own worktree on its own branch.
    for name in &names {
        let branch = f.yard.branch(name).unwrap();
        let on = git(&branch.info().worktree, &["branch", "--show-current"]);
        assert_eq!(on.trim(), format!("by/{name}"));
    }
    assert_eq!(setups(&f), 1);
}

/// Wait until `ready` holds, at most a minute.
fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_keeper_refills_after_a_claim_without_holding_it_up() {
    let f = fixture("v1");
    let spec = spec(1);
    let fills: Arc<Mutex<Vec<PoolFill>>> = Arc::default();
    let keeper = {
        let spec = spec.clone();
        let fills = fills.clone();
        // A long interval: only a claim wakes it within the test.
        f.yard.keep_pool(
            move || Some(spec.clone()),
            Duration::from_secs(3600),
            move |filled| fills.lock().unwrap().push(filled.clone()),
        )
    };
    wait_for("the first fill", || ready(&f, &spec).len() == 1);
    let first = ready(&f, &spec)[0].clone();
    let a = f
        .yard
        .task("SH cat deps/lib.txt")
        .options(with(&f, spec.clone()))
        .name("a")
        .run()
        .unwrap();
    assert!(finished(&a));
    assert_eq!(pool_use(&a).slot.as_deref(), Some(first.as_str()));
    wait_for("a refill", || {
        let now = ready(&f, &spec);
        now.len() == 1 && now[0] != first
    });
    keeper.stop();
    let fills = fills.lock().unwrap();
    let made: usize = fills.iter().map(|f| f.made.len()).sum();
    assert_eq!(made, 2, "{fills:?}");
    assert!(fills.iter().all(|f| f.error.is_none()), "{fills:?}");
}

#[test]
fn slots_go_stale_when_setup_changes_the_base_moves_or_they_age() {
    let f = fixture("v1");
    let spec = spec(1);
    let first = f.yard.fill_pool(&spec).unwrap().made.remove(0);

    // Setup changed: a new pool, and the old one's slot is discarded.
    let mut changed = spec.clone();
    changed.setup.push("true".into());
    let refilled = f.yard.fill_pool(&changed).unwrap();
    assert_eq!(refilled.discarded.len(), 1, "{refilled:?}");
    assert_eq!(refilled.discarded[0].0, first.id);
    assert!(
        refilled.discarded[0].1.contains("setup changed"),
        "{refilled:?}"
    );
    assert_eq!(refilled.made.len(), 1);
    assert!(!first.path.exists());

    // The base moved by a commit that changes no input: the slot is brought
    // forward when a branch takes it.
    let slot = refilled.made[0].clone();
    fs::write(f.root.join("b.txt"), "b\n").unwrap();
    f.git(&["add", "b.txt"]);
    f.git(&["commit", "-q", "-m", "b"]);
    let a = f
        .yard
        .task("SH cat b.txt deps/lib.txt")
        .options(with(&f, changed.clone()))
        .name("a")
        .run()
        .unwrap();
    assert!(finished(&a), "{:?}", a.info().status);
    let used = pool_use(&a);
    assert_eq!(used.slot.as_deref(), Some(slot.id.as_str()), "{used:?}");
    assert_eq!(used.reason.as_deref(), Some("brought forward 1 commit"));
    assert!(text(&a.events().unwrap()).contains("b\nlib-v1"));
    assert_eq!(head(&a.info().worktree), head(&f.root));

    // A commit that changes an input (a new environment key) makes a slot
    // behind it stale.
    let slot = f.yard.fill_pool(&changed).unwrap().made.remove(0);
    fs::write(f.root.join("pnpm-lock.yaml"), "v2").unwrap();
    f.git(&["commit", "-q", "-am", "lock"]);
    let b = f
        .yard
        .task("SH cat deps/lib.txt")
        .options(with(&f, changed.clone()))
        .name("b")
        .run()
        .unwrap();
    assert!(finished(&b), "{:?}", b.info().status);
    assert!(text(&b.events().unwrap()).contains("lib-v2"));
    let used = pool_use(&b);
    assert_eq!(used.slot, None);
    let why = used.reason.unwrap();
    assert!(why.contains("inputs"), "{why}");
    // Taken for removal by the claim, removed by the next fill.
    let next = f.yard.fill_pool(&changed).unwrap();
    assert!(
        next.reclaimed.iter().any(|(id, _)| id == &slot.id),
        "{next:?}"
    );
    assert!(!slot.path.exists());

    // Beyond max_behind.
    let mut strict = changed.clone();
    strict.pool.as_mut().unwrap().max_behind = Some(0);
    let slot = next.made[0].clone();
    fs::write(f.root.join("c.txt"), "c\n").unwrap();
    f.git(&["add", "c.txt"]);
    f.git(&["commit", "-q", "-m", "c"]);
    let moved = f.yard.fill_pool(&strict).unwrap();
    assert_eq!(moved.discarded.len(), 1, "{moved:?}");
    assert_eq!(moved.discarded[0].0, slot.id);
    assert!(moved.discarded[0].1.contains("allows 0"), "{moved:?}");
    let slot = moved.made[0].clone();
    assert_eq!(slot.base, head(&f.root));

    // Aged out: ready for longer than max_age.
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    db.execute(
        "UPDATE pool_slots SET changed_ms = 1 WHERE id = ?1",
        [&slot.id],
    )
    .unwrap();
    let aged = f.yard.fill_pool(&strict).unwrap();
    assert_eq!(aged.discarded.len(), 1, "{aged:?}");
    assert!(aged.discarded[0].1.contains("max_age"), "{aged:?}");
    assert_eq!(aged.made.len(), 1);
    assert_eq!(pool_worktrees(&f).len(), 1);
}

#[test]
fn a_dirty_slot_is_never_handed_out() {
    let f = fixture("v1");
    let spec = spec(1);
    let slot = f.yard.fill_pool(&spec).unwrap().made.remove(0);
    fs::write(slot.path.join("a.txt"), "edited in the pool\n").unwrap();
    let a = f
        .yard
        .task("SH cat a.txt")
        .options(with(&f, spec.clone()))
        .name("a")
        .run()
        .unwrap();
    assert!(finished(&a), "{:?}", a.info().status);
    let used = pool_use(&a);
    assert_eq!(used.slot, None);
    assert!(used.reason.unwrap().contains("not clean"));
    assert!(text(&a.events().unwrap()).contains("one\ntwo"));
    assert!(!slot.path.exists());
    assert!(f.yard.pool_slots().unwrap().is_empty());
}

/// Fill the pool of the repository at `BY_POOL_ROOT` with a setup that
/// stops (`BY_POOL_SETUP`). Run only as the child of the recovery test,
/// which kills it.
#[test]
#[ignore = "the child process of the pool recovery test"]
fn pool_child() {
    let (Some(root), Some(setup)) = (
        std::env::var_os("BY_POOL_ROOT"),
        std::env::var("BY_POOL_SETUP").ok(),
    ) else {
        return;
    };
    let yard = Yard::open(root).unwrap();
    let _ = yard.fill_pool(&WorkspaceSpec {
        setup: vec![setup],
        prepare: true,
        pool: Some(PoolSpec {
            size: 1,
            ..PoolSpec::default()
        }),
        ..WorkspaceSpec::default()
    });
}

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_filler_killed_midway_leaves_nothing_once_recovery_runs() {
    let f = fixture("v1");
    let marker = f.dir.join("started");
    let setup = format!("touch {}; sleep 120", marker.display());
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["pool_child", "--exact", "--ignored", "--test-threads=1"])
        .env("BY_POOL_ROOT", &f.root)
        .env("BY_POOL_SETUP", &setup)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = Killed(child);
    wait_for("setup in the slot", || marker.exists());
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    // The killed filler's slot is recorded, and on disk.
    let slots = f.yard.pool_slots().unwrap();
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].state, PoolSlotState::Filling);
    assert!(slots[0].path.is_dir());
    // An orphan: a directory in the pool with no record.
    let orphan = f.root.join(".branchyard/pool/sorphan");
    git(
        &f.root,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            orphan.to_str().unwrap(),
            "HEAD",
        ],
    );

    // Opening recovers: both are gone, from disk and from git.
    let yard = Yard::open(&f.root).unwrap();
    assert!(yard.pool_slots().unwrap().is_empty());
    assert!(!slots[0].path.exists());
    assert!(!orphan.exists());
    assert!(pool_worktrees(&f).is_empty(), "{:?}", pool_worktrees(&f));
    // And the pool fills again from scratch.
    let filled = yard.fill_pool(&spec(1)).unwrap();
    assert_eq!(filled.made.len(), 1, "{filled:?}");
}

#[test]
fn a_claim_whose_process_stopped_is_reclaimed_not_handed_out_again() {
    let f = fixture("v1");
    let spec = spec(2);
    let filled = f.yard.fill_pool(&spec).unwrap();
    let (claimed, kept) = (&filled.made[0], &filled.made[1]);
    // A claim by a process that is gone: as an engine killed between taking
    // the slot and moving its worktree leaves it.
    let gone = Command::new("true").spawn().unwrap().id();
    let db = rusqlite::Connection::open(f.root.join(".branchyard/state.db")).unwrap();
    db.execute(
        "UPDATE pool_slots SET state = 'claimed', branch = 'lost', pid = ?2 WHERE id = ?1",
        rusqlite::params![claimed.id, gone],
    )
    .unwrap();
    let yard = Yard::open(&f.root).unwrap();
    let left: Vec<String> = yard
        .pool_slots()
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(left, std::slice::from_ref(&kept.id));
    assert!(!claimed.path.exists());
    // The ready one is still claimable, once.
    let a = yard
        .task("SH true")
        .options(with(&f, spec.clone()))
        .name("a")
        .run()
        .unwrap();
    assert_eq!(pool_use(&a).slot.as_deref(), Some(kept.id.as_str()));
    let b = yard
        .task("SH true")
        .options(with(&f, spec.clone()))
        .name("b")
        .run()
        .unwrap();
    assert_eq!(pool_use(&b).slot, None);
}
