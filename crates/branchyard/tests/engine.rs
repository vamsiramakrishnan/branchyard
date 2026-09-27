//! The engine end to end against the fake ACP agent and temporary git
//! repositories.

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use branchyard::{
    Activity, BranchEvent, BranchStatus, Budget, DecisionSource, Error, Event, PermissionDecision,
    Policy, RecordedEvent, TaskOptions, TurnOutcome, Yard,
};
use common::{fake_agent, text, Fixture};

fn status_of(fixture: &Fixture, name: &str) -> BranchStatus {
    fixture.yard.branch(name).unwrap().info().status.clone()
}

#[test]
fn a_run_that_writes_files_is_ready_with_its_diffstat() {
    let f = Fixture::new();
    let branch = f
        .task("WRITE hello.txt=hi WRITE src/deep.txt=x")
        .name("hello")
        .run()
        .unwrap();
    let info = branch.info();
    assert_eq!(info.status, BranchStatus::Ready);
    assert_eq!(info.git_branch, "by/hello");
    assert_eq!(
        (info.harness.as_str(), info.profile.as_str()),
        ("gemini-cli", "gemini-cli-acp")
    );
    assert_eq!(info.session.as_deref(), Some("fake-session-1"));
    assert_eq!(info.turns, 1);
    assert_eq!(info.cost_usd, None, "ACP reports no cost");
    assert_eq!(info.base, f.git(&["rev-parse", "main"]).trim());
    let candidate = info.candidate.as_ref().unwrap();
    assert_eq!(
        (
            candidate.files_changed,
            candidate.insertions,
            candidate.deletions
        ),
        (2, 2, 0)
    );
    assert_eq!(
        f.git(&["rev-parse", "by/hello"]).trim(),
        candidate.commit,
        "the candidate is the branch head"
    );
    assert!(info.worktree.join("hello.txt").is_file());
    let diff = branch.diff().unwrap();
    assert!(
        diff.contains("+++ b/hello.txt") && diff.contains("+hi"),
        "{diff}"
    );
    assert!(diff.contains("+++ b/src/deep.txt"), "{diff}");
    // The user's checkout is untouched and .branchyard/ is invisible to git.
    assert_eq!(
        f.git(&["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
    assert!(!f.root.join("hello.txt").exists());
    assert!(!f.root.join(".gitignore").exists());
}

#[test]
fn a_turn_without_changes_has_no_candidate() {
    let f = Fixture::new();
    let branch = f.task("just say something").run().unwrap();
    let info = branch.info();
    assert_eq!(info.name, "just-say-something");
    assert_eq!(info.status, BranchStatus::NoChanges);
    assert_eq!(info.candidate, None);
    assert_eq!(branch.diff().unwrap(), "");
    assert_eq!(text(&branch.events().unwrap()), "echo: just say something");
    assert!(matches!(
        f.yard.merge("just-say-something", "main"),
        Err(Error::NoCandidate(name)) if name == "just-say-something"
    ));
}

#[test]
fn run_on_runs_branches_in_parallel_with_unique_planned_names() {
    let f = Fixture::new();
    let seen: Arc<Mutex<Vec<BranchEvent>>> = Arc::default();
    let sink = seen.clone();
    let prompt = "WRITE f.txt=x";
    let planned = f
        .task(prompt)
        .planned_names(&["gemini-cli", "qwen-code"])
        .unwrap();
    assert_eq!(
        planned,
        ["write-f-txt-x-gemini-cli", "write-f-txt-x-qwen-code"]
    );
    let branches = f
        .task(prompt)
        .on_event(move |event| sink.lock().unwrap().push(event.clone()))
        .run_on(&["gemini-cli", "qwen-code"])
        .unwrap();
    let names: Vec<&str> = branches.iter().map(|b| b.info().name.as_str()).collect();
    assert_eq!(names, planned);
    assert_eq!(branches[1].info().profile, "qwen-code-acp");
    for branch in &branches {
        assert_eq!(branch.info().status, BranchStatus::Ready);
        assert_eq!(branch.info().candidate.as_ref().unwrap().files_changed, 1);
    }
    assert_ne!(
        branches[0].info().candidate.as_ref().unwrap().commit,
        branches[1].info().candidate.as_ref().unwrap().commit
    );

    // The observer saw both branches, each ending with its final status.
    let seen = seen.lock().unwrap();
    let observed: BTreeSet<&str> = seen.iter().map(|e| e.branch.as_str()).collect();
    assert_eq!(observed, names.iter().copied().collect());
    for name in &names {
        let last = seen.iter().rev().find(|e| e.branch == *name).unwrap();
        assert_eq!(last.activity, Activity::Status(BranchStatus::Ready));
    }

    // The same task again gets the next free names, as planned.
    let again = f
        .task(prompt)
        .planned_names(&["gemini-cli", "qwen-code"])
        .unwrap();
    assert_eq!(
        again,
        ["write-f-txt-x-2-gemini-cli", "write-f-txt-x-2-qwen-code"]
    );
    let second = f.task(prompt).run_on(&["gemini-cli", "qwen-code"]).unwrap();
    let second: Vec<&str> = second.iter().map(|b| b.info().name.as_str()).collect();
    assert_eq!(second, again);
    assert_eq!(f.yard.branches().unwrap().len(), 4);
}

#[test]
fn an_unknown_or_missing_harness_creates_nothing() {
    let f = Fixture::new();
    assert!(matches!(
        f.task("x").harness("nope").run(),
        Err(Error::UnknownHarness(id)) if id == "nope"
    ));
    let missing = f
        .task("x")
        .command(["/nonexistent/fake-agent"])
        .run_on(&["gemini-cli", "qwen-code"]);
    assert!(
        matches!(&missing, Err(Error::HarnessUnavailable { harness, .. }) if harness == "gemini-cli"),
        "{missing:?}"
    );
    assert!(f.yard.branches().unwrap().is_empty());
    assert_eq!(f.git(&["branch", "--list", "by/*"]), "");
    assert!(matches!(
        f.task("x").name("Bad/Name").run(),
        Err(Error::InvalidName { .. })
    ));
}

#[test]
fn send_resumes_the_session_in_the_same_worktree() {
    let f = Fixture::new();
    let first = f.task("WHOAMI").name("chat").run().unwrap();
    assert_eq!(first.info().status, BranchStatus::NoChanges);
    assert_eq!(
        text(&first.events().unwrap()),
        "session fake-session-1 resumed=false"
    );

    let second = first
        .send("WHOAMI WRITE notes.txt=more", f.options())
        .unwrap();
    let info = second.info();
    assert_eq!(info.status, BranchStatus::Ready);
    assert_eq!(info.turns, 2);
    assert_eq!(info.session.as_deref(), Some("fake-session-1"));
    let events = second.events().unwrap();
    assert!(
        text(&events).contains("session fake-session-1 resumed=true"),
        "{}",
        text(&events)
    );
    let prompts: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Prompt(p) => Some(p.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(prompts, ["WHOAMI", "WHOAMI WRITE notes.txt=more"]);

    // A third turn without changes keeps the candidate.
    let third = second.send("WHOAMI", f.options()).unwrap();
    assert_eq!(third.info().status, BranchStatus::NoChanges);
    assert_eq!(third.info().candidate, info.candidate);
}

#[test]
fn acp_cannot_fork_a_session_but_can_fork_with_a_fresh_one() {
    let f = Fixture::new();
    let parent = f.task("WRITE base.txt=1").name("parent").run().unwrap();
    let refused = parent.fork("WRITE child.txt=2", false, f.options());
    assert!(
        matches!(&refused, Err(Error::Unsupported(why)) if why.contains("gemini-cli-acp cannot fork")),
        "{refused:?}"
    );
    assert_eq!(f.yard.branches().unwrap().len(), 1, "nothing was created");

    let child = parent
        .fork(
            "WRITE child.txt=2",
            true,
            TaskOptions {
                name: Some("child".into()),
                ..f.options()
            },
        )
        .unwrap();
    let info = child.info();
    assert_eq!(info.status, BranchStatus::Ready);
    assert_eq!(info.parent.as_deref(), Some("parent"));
    assert_eq!(
        info.base,
        parent.info().candidate.as_ref().unwrap().commit,
        "a fork starts from its parent's candidate"
    );
    assert!(info.worktree.join("base.txt").is_file());
    let diff = child.diff().unwrap();
    assert!(
        diff.contains("child.txt") && !diff.contains("base.txt"),
        "{diff}"
    );
    let events = child.events().unwrap();
    assert!(text(&events).contains("wrote child.txt"));

    let empty = f.task("nothing").run().unwrap();
    assert!(matches!(
        empty.fork("x", true, f.options()),
        Err(Error::NoCandidate(name)) if name == "nothing"
    ));
}

#[test]
fn merge_lands_a_merge_commit_on_main() {
    let f = Fixture::new();
    let branch = f
        .task("WRITE feature.txt=done")
        .name("feature")
        .check(["sh", "-c", "test -f feature.txt"])
        .run()
        .unwrap();
    let before = f.git(&["rev-parse", "main"]).trim().to_owned();
    let merged = f.yard.merge("feature", "main").unwrap();
    assert_eq!(merged.previous, before);
    assert_eq!(merged.commit, f.git(&["rev-parse", "main"]).trim());
    let parents = f.git(&["rev-list", "--parents", "-n", "1", "main"]);
    let candidate = &branch.info().candidate.as_ref().unwrap().commit;
    assert_eq!(
        parents.split_whitespace().collect::<Vec<_>>(),
        [merged.commit.as_str(), before.as_str(), candidate.as_str()]
    );
    // The checked-out main moved with it.
    assert_eq!(
        fs::read_to_string(f.root.join("feature.txt")).unwrap(),
        "done\n"
    );
    assert_eq!(f.git(&["status", "--porcelain"]), "");
    assert_eq!(
        status_of(&f, "feature"),
        BranchStatus::Merged {
            target: "main".into(),
            commit: merged.commit.clone()
        }
    );
    let last = branch.events().unwrap().pop().unwrap();
    assert!(matches!(
        last.activity,
        Activity::Status(BranchStatus::Merged { .. })
    ));
    assert!(matches!(
        f.yard.merge("feature", "main"),
        Err(Error::AlreadyMerged { target }) if target == "main"
    ));
    assert!(matches!(
        f.yard.merge("feature", "nope"),
        Err(Error::Git(_))
    ));
}

#[test]
fn a_target_that_moves_during_the_check_is_refused() {
    let f = Fixture::new();
    let root = f.root.display().to_string();
    let mover = format!("git -C '{root}' commit -q --allow-empty -m moved");
    f.task("WRITE m.txt=1")
        .name("moving")
        .check(["sh", "-c", &mover])
        .run()
        .unwrap();
    let before = f.git(&["rev-parse", "main"]).trim().to_owned();
    let result = f.yard.merge("moving", "main");
    let after = f.git(&["rev-parse", "main"]).trim().to_owned();
    assert_ne!(before, after);
    match result {
        Err(Error::TargetMoved { expected, actual }) => {
            assert_eq!(expected, before);
            assert_eq!(actual.as_deref(), Some(after.as_str()));
        }
        other => panic!("expected TargetMoved, got {other:?}"),
    }
    assert_eq!(status_of(&f, "moving"), BranchStatus::Ready);
}

#[test]
fn a_failing_check_blocks_the_merge() {
    let f = Fixture::new();
    f.task("WRITE c.txt=1")
        .name("checked")
        .check(["sh", "-c", "echo nope; exit 1"])
        .run()
        .unwrap();
    let before = f.git(&["rev-parse", "main"]);
    match f.yard.merge("checked", "main") {
        Err(Error::CheckFailed { output_tail }) => assert!(output_tail.contains("nope")),
        other => panic!("expected CheckFailed, got {other:?}"),
    }
    assert_eq!(f.git(&["rev-parse", "main"]), before);
    assert_eq!(status_of(&f, "checked"), BranchStatus::Ready);
}

#[test]
fn conflicting_candidates_are_returned_with_their_files() {
    let f = Fixture::new();
    f.task("WRITE same.txt=left").name("left").run().unwrap();
    f.task("WRITE same.txt=right").name("right").run().unwrap();
    f.yard.merge("left", "main").unwrap();
    match f.yard.merge("right", "main") {
        Err(Error::Conflict { files }) => assert_eq!(files, ["same.txt"]),
        other => panic!("expected Conflict, got {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(f.root.join("same.txt")).unwrap(),
        "left\n"
    );
}

fn decisions(events: &[RecordedEvent]) -> Vec<(String, bool, DecisionSource)> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Decision {
                tool,
                allowed,
                source,
                ..
            } => Some((tool.clone(), *allowed, source.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn permission_decisions_are_recorded_and_the_default_denies() {
    let f = Fixture::new();
    let denied = f
        .task("PERMISSION WRITE p.txt=1")
        .name("denied")
        .run()
        .unwrap();
    assert_eq!(denied.info().status, BranchStatus::NoChanges);
    assert!(!denied.info().worktree.join("p.txt").exists());
    let events = denied.events().unwrap();
    assert_eq!(
        decisions(&events),
        [("write marker".to_owned(), false, DecisionSource::Default)]
    );
    // The decision follows its request, and the harness saw the denial.
    let asked = events
        .iter()
        .position(|e| {
            matches!(
                e.activity,
                Activity::Harness(Event::PermissionRequested { .. })
            )
        })
        .unwrap();
    assert!(matches!(
        events[asked + 1].activity,
        Activity::Decision { .. }
    ));
    assert!(text(&events).ends_with("denied"));

    let allowed = f
        .task("PERMISSION WRITE p.txt=1")
        .name("allowed")
        .policy(Policy::deny_all().allow("write *"))
        .run()
        .unwrap();
    assert_eq!(allowed.info().status, BranchStatus::Ready);
    assert!(allowed.info().worktree.join("p.txt").is_file());
    assert_eq!(
        decisions(&allowed.events().unwrap()),
        [(
            "write marker".to_owned(),
            true,
            DecisionSource::Rule {
                pattern: "write *".into()
            }
        )]
    );

    let asked: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = asked.clone();
    let branch = f
        .task("PERMISSION")
        .name("asked")
        .policy(Policy::ask(move |branch, request| {
            log.lock()
                .unwrap()
                .push(format!("{branch}:{}", request.tool));
            PermissionDecision::Allow
        }))
        .run()
        .unwrap();
    assert_eq!(*asked.lock().unwrap(), ["asked:write marker"]);
    assert_eq!(
        decisions(&branch.events().unwrap()),
        [("write marker".to_owned(), true, DecisionSource::Asked)]
    );
}

#[test]
fn max_turns_stops_a_send_before_it_starts() {
    let f = Fixture::new();
    let branch = f.task("WHOAMI").name("limited").run().unwrap();
    let before = branch.events().unwrap().len();
    let options = TaskOptions {
        budget: Budget::default().turns(1),
        ..f.options()
    };
    let over = branch.send("WHOAMI", options).unwrap();
    assert_eq!(
        over.info().status,
        BranchStatus::BudgetExceeded {
            limit: "max_turns".into()
        }
    );
    assert_eq!(over.info().turns, 1);
    let events = over.events().unwrap();
    let new: Vec<&Activity> = events[before..].iter().map(|e| &e.activity).collect();
    assert_eq!(
        new,
        [
            &Activity::Status(BranchStatus::Running),
            &Activity::Status(BranchStatus::BudgetExceeded {
                limit: "max_turns".into()
            })
        ],
        "no harness was started"
    );
}

#[test]
fn a_duration_budget_interrupts_a_hanging_turn() {
    let f = Fixture::new();
    let started = Instant::now();
    let branch = f
        .task("HANG WRITE partial.txt=1")
        .name("hangs")
        .budget(Budget::default().duration(Duration::from_millis(300)))
        .run()
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        branch.info().status,
        BranchStatus::BudgetExceeded {
            limit: "max_duration".into()
        }
    );
    // The harness ended the turn itself after the interrupt.
    let events = branch.events().unwrap();
    assert!(events.iter().any(|e| e.activity
        == Activity::Harness(Event::TurnEnded {
            turn: 1,
            outcome: TurnOutcome::Interrupted
        })));
}

#[test]
fn a_harness_that_exits_mid_turn_fails_the_branch() {
    let f = Fixture::new();
    let branch = f.task("EXIT").name("exits").run().unwrap();
    match &branch.info().status {
        BranchStatus::Failed { reason } => {
            assert!(reason.contains("outcome is unknown"), "{reason}")
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn descendants_that_outlive_the_harness_are_killed_and_recorded() {
    let f = Fixture::new();
    let branch = f.task("BACKGROUND").name("background").run().unwrap();
    let events = branch.events().unwrap();
    let pid: u32 = text(&events)
        .strip_prefix("background pid ")
        .and_then(|pid| pid.trim().parse().ok())
        .unwrap();
    assert!(
        events.iter().any(|e| matches!(
            &e.activity,
            Activity::Warning(w) if w.contains("outlived the harness") && w.contains("sleep")
        )),
        "{events:?}"
    );
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    assert!(
        stat.trim().is_empty() || stat.trim().starts_with('Z'),
        "{stat}"
    );
}

#[test]
fn the_event_log_round_trips_and_reloads_in_a_new_yard() {
    let f = Fixture::new();
    let observed: Arc<Mutex<Vec<Activity>>> = Arc::default();
    let sink = observed.clone();
    let branch = f
        .task("PERMISSION WRITE r.txt=1")
        .name("logged")
        .policy(Policy::allow_all())
        .on_event(move |event| sink.lock().unwrap().push(event.activity.clone()))
        .run()
        .unwrap();
    let events = branch.events().unwrap();
    let activities: Vec<Activity> = events.iter().map(|e| e.activity.clone()).collect();
    assert_eq!(
        activities,
        *observed.lock().unwrap(),
        "observed as recorded"
    );
    assert!(events.windows(2).all(|w| w[0].at_ms <= w[1].at_ms));
    assert_eq!(
        activities.first(),
        Some(&Activity::Status(BranchStatus::Running))
    );
    assert_eq!(
        activities.last(),
        Some(&Activity::Status(BranchStatus::Ready))
    );
    for expected in [
        Activity::Harness(Event::Ready),
        Activity::Prompt("PERMISSION WRITE r.txt=1".into()),
        Activity::Harness(Event::SessionClosed),
    ] {
        assert!(activities.contains(&expected), "{expected:?}");
    }
    assert!(activities
        .iter()
        .any(|a| matches!(a, Activity::Snapshot(c) if c.files_changed == 1)));

    let reopened = Yard::open(f.root.join("a.txt").parent().unwrap()).unwrap();
    assert_eq!(reopened.branch("logged").unwrap().events().unwrap(), events);
    assert_eq!(reopened.branches().unwrap(), f.yard.branches().unwrap());
    let line = serde_json::to_string(&events[1]).unwrap();
    assert_eq!(
        serde_json::from_str::<RecordedEvent>(&line).unwrap(),
        events[1]
    );
}

#[test]
fn state_is_excluded_from_git_once_even_from_a_linked_worktree() {
    let f = Fixture::new();
    let linked = f.dir.join("linked");
    f.git(&["worktree", "add", "-q", linked.to_str().unwrap()]);
    let from_linked = Yard::open(&linked).unwrap();
    Yard::open(&f.root).unwrap();
    assert_eq!(from_linked.root(), linked);
    let exclude = fs::read_to_string(f.root.join(".git/info/exclude")).unwrap();
    assert_eq!(exclude.matches(".branchyard/").count(), 1, "{exclude}");
    assert_eq!(
        common::git(&linked, &["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
    assert_eq!(
        f.git(&["status", "--porcelain", "--untracked-files=all"]),
        ""
    );
    assert!(matches!(Yard::open(&f.dir), Err(Error::NotARepository(_))));
}

#[test]
fn remove_deletes_worktree_record_and_unmerged_branch() {
    let f = Fixture::new();
    let branch = f.task("WRITE gone.txt=1").name("gone").run().unwrap();
    let worktree = branch.info().worktree.clone();
    f.yard.remove("gone").unwrap();
    assert!(!worktree.exists());
    assert_eq!(f.git(&["branch", "--list", "by/gone"]), "");
    assert!(matches!(
        f.yard.branch("gone"),
        Err(Error::UnknownBranch(_))
    ));
    assert!(f.yard.branches().unwrap().is_empty());
    assert!(matches!(
        f.yard.remove("gone"),
        Err(Error::UnknownBranch(_))
    ));

    // A merged branch keeps its git branch, which keeps its name taken.
    f.task("WRITE kept.txt=1").name("kept").run().unwrap();
    f.yard.merge("kept", "main").unwrap();
    f.yard.remove("kept").unwrap();
    assert_eq!(f.git(&["branch", "--list", "by/kept"]).trim(), "by/kept");
    assert!(matches!(
        f.task("x").name("kept").run(),
        Err(Error::BranchExists(name)) if name == "kept"
    ));
    assert_eq!(
        f.task("kept").planned_names(&[]).unwrap(),
        ["kept-2"],
        "automatic names skip it"
    );
}

#[test]
fn harnesses_lists_every_profile_with_availability_and_qualification() {
    let f = Fixture::new();
    let harnesses = f.yard.harnesses();
    assert_eq!(
        harnesses.len(),
        branchyard_harness::profiles::PROFILES.len()
    );
    let stream = harnesses
        .iter()
        .find(|h| h.profile == "claude-code-stream-json")
        .unwrap();
    assert!(stream.default);
    assert_eq!(stream.harness, "claude-code");
    assert!(stream
        .qualification
        .as_deref()
        .unwrap()
        .starts_with("9/9 on 2.1.283"));
    let acp = harnesses
        .iter()
        .find(|h| h.profile == "claude-code-acp")
        .unwrap();
    assert!(!acp.default);
    let codex = harnesses
        .iter()
        .find(|h| h.profile == "codex-app-server")
        .unwrap();
    assert!(codex.default && codex.qualification.is_none());
    let path = std::env::var_os("PATH").unwrap_or_default();
    for harness in &harnesses {
        let program = branchyard_harness::profiles::by_id(&harness.profile)
            .unwrap()
            .command[0];
        let found = std::env::split_paths(&path).any(|dir| dir.join(program).is_file());
        assert_eq!(harness.available, found, "{}", harness.profile);
    }
}

#[test]
fn harnesses_inherit_the_environment_without_nested_session_markers() {
    let f = Fixture::new();
    let prompt = "ENV CLAUDECODE BRANCHYARD_TEST_VISIBLE";
    let shared = f.task(prompt).name("shared").run().unwrap();
    let home = std::env::var("HOME").unwrap();
    assert_eq!(
        text(&shared.events().unwrap()),
        format!("HOME={home}\nCLAUDECODE unset\nBRANCHYARD_TEST_VISIBLE=yes\n")
    );

    let isolated = f
        .task(prompt)
        .name("isolated")
        .isolated(true)
        .run()
        .unwrap();
    let private = f.root.join(".branchyard/homes/isolated");
    assert!(private.is_dir());
    assert_eq!(
        text(&isolated.events().unwrap()),
        format!(
            "HOME={}\nCLAUDECODE unset\nBRANCHYARD_TEST_VISIBLE=yes\n",
            private.display()
        )
    );
    f.yard.remove("isolated").unwrap();
    assert!(!private.exists());
    assert!(fake_agent().is_file());
}

#[test]
fn a_session_the_harness_cannot_find_fails_precisely() {
    let f = Fixture::new();
    let branch = f.task("WHOAMI").name("lost").run().unwrap();
    // Stand in for a harness that no longer has the session.
    let path = f.root.join(".branchyard/branches/lost.json");
    let mut record: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    record["info"]["session"] = "missing-1".into();
    fs::write(&path, record.to_string()).unwrap();

    let sent = branch.send("WHOAMI", f.options()).unwrap();
    let info = sent.info();
    match &info.status {
        BranchStatus::Failed { reason } => {
            let expected = format!(
                "could not resume session missing-1 in {}: open failed: Resource not found: missing-1",
                info.worktree.display()
            );
            assert_eq!(reason, &expected);
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(info.turns, 1, "no turn was submitted");
    assert_eq!(info.session.as_deref(), Some("missing-1"));
}
