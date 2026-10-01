//! Plan approval and goals a judge verifies, against the fake ACP agent.
//! No model is called: a plan is whatever the fake agent replies, and a
//! goal's judge is either a canned verdict file the fake agent answers or a
//! judge of the test's own that answers a scripted sequence.

mod common;

use std::sync::{Arc, Mutex};

use branchyard::{
    Activity, BranchStatus, Budget, Envelope, Error, Goal, GoalActivity, Judge, JudgeSpec,
    JudgedBy, PlanActivity, PlanPhase, Policy, RecordedEvent, Spawn, TaskOptions, Yard,
};
use common::{fake_agent, Fixture};

fn prompts(events: &[RecordedEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Prompt(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn plan_activity(events: &[RecordedEvent]) -> Vec<PlanActivity> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Plan(a) => Some(a.as_ref().clone()),
            _ => None,
        })
        .collect()
}

fn goal_activity(events: &[RecordedEvent]) -> Vec<GoalActivity> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Goal(a) => Some(a.as_ref().clone()),
            _ => None,
        })
        .collect()
}

fn planning(f: &Fixture) -> TaskOptions {
    TaskOptions {
        plan: true,
        // Plan mode overrides even a policy that allows everything.
        policy: Policy::allow_all(),
        ..f.options()
    }
}

#[test]
fn a_plan_is_written_read_only_then_approved_with_edits() {
    let f = Fixture::new();
    let branch = f
        .task("Write the marker PERMISSION WRITE marker.txt=x")
        .options(planning(&f))
        .run()
        .unwrap();
    let name = branch.info().name.clone();
    assert_eq!(branch.info().status, BranchStatus::AwaitingPlanApproval);
    // The task stays the task; the turn got the planning prompt.
    assert_eq!(
        branch.info().prompt,
        "Write the marker PERMISSION WRITE marker.txt=x"
    );
    let events = branch.events().unwrap();
    assert!(prompts(&events)[0].starts_with("[branchyard plan mode]"));
    // Read-only: the write was asked for and denied, though the caller's
    // policy allows everything.
    let decisions: Vec<bool> = events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Decision { allowed, .. } => Some(*allowed),
            _ => None,
        })
        .collect();
    assert_eq!(decisions, [false]);
    assert!(!branch.info().worktree.join("marker.txt").exists());
    assert!(branch.info().candidate.is_none());
    let info = f.yard.plan(&name).unwrap();
    assert_eq!(info.phase, PlanPhase::Awaiting);
    assert_eq!(info.round, 1);
    assert_eq!(info.plan.as_ref().unwrap().markdown, "denied");
    assert!(matches!(
        plan_activity(&events)[..],
        [
            PlanActivity::Planning { round: 1 },
            PlanActivity::Proposed(_)
        ]
    ));
    // The same, from events alone, as a surface without the record sees it.
    assert_eq!(
        branchyard::plan_from_events(&name, &events).unwrap().phase,
        PlanPhase::Awaiting
    );

    // Nothing else may be sent while it waits.
    let refused = branch.send("WRITE early.txt=1", f.options());
    assert!(matches!(refused, Err(Error::Denied(_))), "{refused:?}");

    // Approval with an edited plan runs it, with the caller's policy.
    let done = f
        .yard
        .approve_plan(
            &name,
            Some("Write it: WRITE done.txt=yes"),
            "ana",
            &f.options(),
        )
        .unwrap();
    assert_eq!(done.info().status, BranchStatus::Ready);
    assert!(done.info().worktree.join("done.txt").is_file());
    let events = done.events().unwrap();
    let last = prompts(&events).pop().unwrap();
    assert!(last.starts_with("[branchyard plan approved]"), "{last}");
    assert!(last.contains("Write it: WRITE done.txt=yes"));
    assert!(plan_activity(&events).contains(&PlanActivity::Approved {
        by: "ana".into(),
        edited: true,
        round: 1
    }));
    assert_eq!(f.yard.plan(&name).unwrap().phase, PlanPhase::Approved);
    // Approved once: there is nothing left to approve.
    let again = f.yard.approve_plan(&name, None, "ana", &f.options());
    assert!(matches!(again, Err(Error::NoPlan(_))), "{again:?}");
    // A plain send works again.
    let sent = done.send("WRITE later.txt=1", f.options()).unwrap();
    assert_eq!(sent.info().status, BranchStatus::Ready);
}

#[test]
fn a_rejected_plan_is_written_again_or_ends_the_branch() {
    let f = Fixture::new();
    let plan = f.dir.join("plan.md");
    std::fs::write(
        &plan,
        "1. Read a.txt\n2. Change it\n```json\n{\"tasks\": [{\"title\": \"Read a.txt\"}, \
         {\"title\": \"Change it\", \"detail\": \"one line\"}]}\n```",
    )
    .unwrap();
    let branch = f
        .task(&format!("Change a.txt\nREPLY_FILE {}", plan.display()))
        .options(planning(&f))
        .run()
        .unwrap();
    let name = branch.info().name.clone();
    let proposed = f.yard.plan(&name).unwrap().plan.unwrap();
    let tasks = proposed.tasks.unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[1].detail.as_deref(), Some("one line"));

    // Re-plan: another read-only turn, with the reason, round 2.
    let replanned = f
        .yard
        .reject_plan(&name, Some("smaller steps"), true, "ana", &f.options())
        .unwrap();
    assert_eq!(replanned.info().status, BranchStatus::AwaitingPlanApproval);
    let info = f.yard.plan(&name).unwrap();
    assert_eq!((info.phase, info.round), (PlanPhase::Awaiting, 2));
    let events = replanned.events().unwrap();
    let last = prompts(&events).pop().unwrap();
    assert!(last.starts_with("[branchyard plan mode]") && last.contains("smaller steps"));
    // The re-plan's reply is round 2's plan.
    assert!(info.plan.unwrap().markdown.starts_with("echo:"));

    // Rejected outright: the branch ends, and cannot be approved.
    let ended = f
        .yard
        .reject_plan(&name, Some("not now"), false, "ana", &f.options())
        .unwrap();
    match &ended.info().status {
        BranchStatus::Failed { reason } => {
            assert!(reason.contains("rejected by ana: not now"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(f.yard.plan(&name).unwrap().phase, PlanPhase::Rejected);
    assert!(matches!(
        f.yard.approve_plan(&name, None, "ana", &f.options()),
        Err(Error::NoPlan(_))
    ));
    assert!(plan_activity(&ended.events().unwrap())
        .iter()
        .any(|a| matches!(a, PlanActivity::Rejected { replan: false, .. })));
}

#[test]
fn plan_mode_needs_a_harness_whose_tools_the_policy_answers() {
    let f = Fixture::new();
    let options = TaskOptions {
        harness: Some("pi".into()),
        unapproved_tools: true,
        plan: true,
        ..f.options()
    };
    let refused = f.task("WRITE a.txt=2").options(options).run();
    assert!(matches!(refused, Err(Error::Unsupported(_))), "{refused:?}");
    assert!(f.yard.branches().unwrap().is_empty());
}

#[test]
fn a_childs_plan_is_escalated_to_its_parent_which_approves_it() {
    let f = Fixture::new();
    let options = TaskOptions {
        delegation: Some(Envelope::depth(1)),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        ..f.options()
    };
    let root = f
        .yard
        .task("WRITE root.txt=r")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options.clone()).unwrap();
    delegate
        .spawn(Spawn {
            prompt: "Change it PERMISSION WRITE kid.txt=x".into(),
            name: Some("kid".into()),
            plan: true,
            ..Spawn::default()
        })
        .unwrap();
    root.wait_subtree().unwrap();
    let kid = f.yard.branch("kid").unwrap();
    assert_eq!(kid.info().status, BranchStatus::AwaitingPlanApproval);
    assert!(!kid.info().worktree.join("kid.txt").exists());
    // The plan went to the parent's inbox as an escalation.
    let inbox = delegate.inbox().unwrap();
    let escalation = inbox
        .messages
        .iter()
        .find(|m| m.from == "kid")
        .expect("the child escalated its plan");
    assert_eq!(escalation.kind, branchyard::MessageKind::Escalation);
    assert!(escalation.text.contains("awaits your approval"));
    assert!(plan_activity(&kid.events().unwrap())
        .iter()
        .any(|a| matches!(a, PlanActivity::Escalated { to, .. } if to == "root")));

    // A branch that is not its ancestor may not decide it.
    let other = f
        .yard
        .task("WRITE other.txt=o")
        .options(options.clone())
        .name("other")
        .run()
        .unwrap();
    let outsider = other.delegate(options.clone()).unwrap();
    assert!(matches!(
        outsider.approve_plan("kid", None),
        Err(Error::Denied(_))
    ));

    // The parent approves an edited plan; the child's turn runs it.
    delegate
        .approve_plan("kid", Some("WRITE kid.txt=approved"))
        .unwrap();
    root.wait_subtree().unwrap();
    let kid = f.yard.branch("kid").unwrap();
    assert_eq!(kid.info().status, BranchStatus::Ready);
    assert!(kid.info().worktree.join("kid.txt").is_file());
    assert!(plan_activity(&kid.events().unwrap())
        .iter()
        .any(|a| matches!(a, PlanActivity::Approved { by, edited: true, .. } if by == "root")));
}

/// A judge of the test's own: answers a scripted sequence of verdicts, and
/// counts how often it was asked.
struct Scripted {
    answers: Mutex<Vec<String>>,
    asked: Mutex<Vec<String>>,
}

impl Scripted {
    fn new(answers: &[&str]) -> Arc<Scripted> {
        Arc::new(Scripted {
            answers: Mutex::new(answers.iter().rev().map(|a| (*a).to_owned()).collect()),
            asked: Mutex::new(Vec::new()),
        })
    }
}

impl Judge for Scripted {
    fn name(&self) -> String {
        "scripted".into()
    }

    fn verdict(&self, _: &Yard, prompt: &str, _: &[String]) -> Result<String, Error> {
        self.asked.lock().unwrap().push(prompt.to_owned());
        Ok(self
            .answers
            .lock()
            .unwrap()
            .pop()
            .expect("a scripted answer"))
    }
}

fn with_goal(f: &Fixture, goal: Goal) -> TaskOptions {
    TaskOptions {
        goal: Some(goal),
        check: Some(vec!["true".into()]),
        ..f.options()
    }
}

#[test]
fn a_goal_met_at_once_records_its_evidence() {
    let f = Fixture::new();
    // No judge: the passing check and the non-empty diff decide.
    let branch = f
        .task("WRITE a.txt=three")
        .options(with_goal(&f, Goal::new("a.txt says three")))
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::Ready);
    let events = branch.events().unwrap();
    let goal = goal_activity(&events);
    assert!(
        matches!(&goal[0], GoalActivity::Set { goal, rounds: 2, .. } if goal == "a.txt says three")
    );
    match &goal[1] {
        GoalActivity::Verdict {
            round: 0,
            met: true,
            evidence,
            by: JudgedBy::Deterministic,
            deterministic,
            ..
        } => {
            assert!(evidence[0].contains("check passed"), "{evidence:?}");
            assert!(deterministic.passed && deterministic.changes);
        }
        other => panic!("{other:?}"),
    }
    let info = f.yard.goal(&branch.info().name).unwrap().unwrap();
    assert_eq!((info.met, info.used), (Some(true), 0));
    // Met: no follow-up turn.
    assert_eq!(prompts(&events).len(), 1);
}

#[test]
fn an_unmet_goal_gets_follow_up_turns_until_the_judge_finds_it_met() {
    let f = Fixture::new();
    let judge = Scripted::new(&[
        r#"{"met": false, "evidence": ["a.txt was written"], "missing": ["b.txt: WRITE b.txt=two"]}"#,
        r#"{"met": true, "evidence": ["a.txt and b.txt exist"], "missing": []}"#,
    ]);
    let goal = Goal {
        custom: Some(judge.clone()),
        ..Goal::new("a.txt and b.txt both exist")
    };
    let branch = f
        .task("WRITE a.txt=one")
        .options(with_goal(&f, goal))
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::Ready);
    assert!(branch.info().worktree.join("b.txt").is_file());
    assert_eq!(branch.info().turns, 2);
    let events = branch.events().unwrap();
    let follow_up = prompts(&events).pop().unwrap();
    assert!(
        follow_up.starts_with("[branchyard goal not met]"),
        "{follow_up}"
    );
    assert!(follow_up.contains("- b.txt: WRITE b.txt=two"));
    let verdicts: Vec<(u32, bool)> = goal_activity(&events)
        .iter()
        .filter_map(|a| match a {
            GoalActivity::Verdict { round, met, .. } => Some((*round, *met)),
            _ => None,
        })
        .collect();
    assert_eq!(verdicts, [(0, false), (1, true)]);
    // The judge saw the goal, the diff and the transcript.
    let asked = judge.asked.lock().unwrap();
    assert_eq!(asked.len(), 2);
    assert!(asked[1].contains("## Goal\na.txt and b.txt both exist"));
    assert!(asked[1].contains("+two"), "the diff is quoted");
    assert!(asked[1].contains("### Turn 2"));
    let info = f.yard.goal(&branch.info().name).unwrap().unwrap();
    assert_eq!((info.met, info.used), (Some(true), 1));
}

#[test]
fn a_goal_whose_rounds_run_out_fails_the_branch() {
    let f = Fixture::new();
    let unmet = |n: u32| {
        format!(r#"{{"met": false, "evidence": [], "missing": ["more: WRITE c{n}.txt=x"]}}"#)
    };
    let (u0, u1, u2) = (unmet(0), unmet(1), unmet(2));
    let judge = Scripted::new(&[&u0, &u1, &u2]);
    let goal = Goal {
        rounds: 2,
        custom: Some(judge.clone()),
        ..Goal::new("never enough")
    };
    let branch = f
        .task("WRITE a.txt=one")
        .options(with_goal(&f, goal))
        .run()
        .unwrap();
    match &branch.info().status {
        BranchStatus::Failed { reason } => {
            assert!(reason.contains("not met after 2 follow-up"), "{reason}");
            assert!(reason.contains("WRITE c2.txt=x"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(branch.info().turns, 3);
    // The candidate stays, for a person to look at or merge.
    assert!(branch.info().candidate.is_some());
    let events = branch.events().unwrap();
    assert!(goal_activity(&events)
        .iter()
        .any(|a| matches!(a, GoalActivity::Exhausted { rounds: 2, .. })));
    let info = branchyard::goal_from_events(&events).unwrap();
    assert_eq!((info.met, info.used), (Some(false), 2));
}

#[test]
fn a_goal_stops_at_the_branchs_budget() {
    let f = Fixture::new();
    let answers: Vec<String> = (0..5)
        .map(|n| format!(r#"{{"met": false, "evidence": [], "missing": ["WRITE d{n}.txt=x"]}}"#))
        .collect();
    let refs: Vec<&str> = answers.iter().map(String::as_str).collect();
    let goal = Goal {
        rounds: 5,
        custom: Some(Scripted::new(&refs)),
        ..Goal::new("unreachable")
    };
    let options = TaskOptions {
        budget: Budget::default().turns(2),
        ..with_goal(&f, goal)
    };
    let branch = f.task("WRITE a.txt=one").options(options).run().unwrap();
    assert!(
        matches!(&branch.info().status, BranchStatus::BudgetExceeded { limit } if limit == "max_turns"),
        "{:?}",
        branch.info().status
    );
    assert_eq!(branch.info().turns, 2);
}

#[test]
fn a_failing_check_is_unmet_without_asking_the_judge() {
    let f = Fixture::new();
    let judge = Scripted::new(&[]);
    let goal = Goal {
        rounds: 0,
        custom: Some(judge.clone()),
        ..Goal::new("tests pass")
    };
    let options = TaskOptions {
        check: Some(vec!["false".into()]),
        ..with_goal(&f, goal)
    };
    let branch = f.task("WRITE a.txt=one").options(options).run().unwrap();
    assert!(matches!(branch.info().status, BranchStatus::Failed { .. }));
    assert!(judge.asked.lock().unwrap().is_empty());
    let events = branch.events().unwrap();
    match goal_activity(&events)
        .into_iter()
        .find(|a| matches!(a, GoalActivity::Verdict { .. }))
    {
        Some(GoalActivity::Verdict {
            met: false,
            missing,
            by: JudgedBy::Deterministic,
            ..
        }) => assert!(missing[0].contains("check passes"), "{missing:?}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_judge_harness_verdict_is_used_and_an_invalid_one_falls_back() {
    let f = Fixture::new();
    let met = f.dir.join("met.json");
    std::fs::write(
        &met,
        r#"{"met": true, "evidence": ["a.txt holds the value"], "missing": []}"#,
    )
    .unwrap();
    let bad = f.dir.join("bad.txt");
    std::fs::write(&bad, "Looks done to me!").unwrap();
    let judged = |rubric: &std::path::Path, name: &str| {
        let goal = Goal {
            judge: Some(JudgeSpec {
                harness: "gemini-cli".into(),
                model: None,
                effort: None,
                command: Some(vec![fake_agent().display().to_string()]),
                rubric: Some(format!("REPLY_FILE {}", rubric.display())),
            }),
            ..Goal::new("a.txt holds the value")
        };
        f.task("WRITE a.txt=value")
            .options(with_goal(&f, goal))
            .name(name)
            .run()
            .unwrap()
    };
    let used = judged(&met, "used");
    assert_eq!(used.info().status, BranchStatus::Ready);
    let verdict = goal_activity(&used.events().unwrap())
        .into_iter()
        .find_map(|a| match a {
            GoalActivity::Verdict { by, evidence, .. } => Some((by, evidence)),
            _ => None,
        })
        .unwrap();
    assert!(matches!(&verdict.0, JudgedBy::Judge { name } if name == "harness gemini-cli"));
    assert_eq!(verdict.1, ["a.txt holds the value"]);

    let fallback = judged(&bad, "fallback");
    // The deterministic result stands, and says why.
    assert_eq!(fallback.info().status, BranchStatus::Ready);
    let by = goal_activity(&fallback.events().unwrap())
        .into_iter()
        .find_map(|a| match a {
            GoalActivity::Verdict { by, met: true, .. } => Some(by),
            _ => None,
        })
        .unwrap();
    match by {
        JudgedBy::Fallback { error, .. } => assert!(error.contains("not a JSON object"), "{error}"),
        other => panic!("{other:?}"),
    }
    // The judge's scratch branches are gone.
    let names: Vec<String> = f
        .yard
        .branches()
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert_eq!(names, ["used", "fallback"]);
}

#[test]
fn a_plan_then_a_goal_verified_after_approval() {
    let f = Fixture::new();
    let judge =
        Scripted::new(&[r#"{"met": true, "evidence": ["done.txt exists"], "missing": []}"#]);
    let options = TaskOptions {
        plan: true,
        goal: Some(Goal {
            custom: Some(judge.clone()),
            ..Goal::new("done.txt exists")
        }),
        check: Some(vec!["true".into()]),
        ..f.options()
    };
    let branch = f
        .task("Make done.txt")
        .options(options.clone())
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::AwaitingPlanApproval);
    // Waiting for approval is not ready: no verdict yet.
    assert!(judge.asked.lock().unwrap().is_empty());
    let done = f
        .yard
        .approve_plan(
            &branch.info().name,
            Some("WRITE done.txt=1"),
            "ana",
            &options,
        )
        .unwrap();
    assert_eq!(done.info().status, BranchStatus::Ready);
    assert_eq!(judge.asked.lock().unwrap().len(), 1);
    assert_eq!(
        f.yard.goal(&done.info().name).unwrap().unwrap().met,
        Some(true)
    );
}
