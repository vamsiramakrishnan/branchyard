//! Structured native outcomes over the fake ACP agent. No model, cloud
//! service or reporting tool is used; knowledge work may leave Git unchanged.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::sync::Arc;

use branchyard::{
    Activity, BranchOutcome, BranchStatus, Error, Fleet, FleetCandidate, FleetEntry, Goal, Judge,
    JudgeOptions, JudgeSpec, JudgedBy, ResultContract, ResultPurpose, RouteOptions, TaskOptions,
};
use common::Fixture;
use serde_json::{json, Value};

fn schema() -> Value {
    json!({"type":"object", "required":["answer"], "additionalProperties":false,
        "properties":{"answer":{"type":"string", "minLength":1}}})
}

fn reply(f: &Fixture, name: &str, body: &str) -> String {
    let path = f.dir.join(format!("{name}.json"));
    std::fs::write(&path, body).unwrap();
    format!("Research the answer\nREPLY_FILE {}", path.display())
}

fn options(f: &Fixture) -> TaskOptions {
    TaskOptions {
        result: Some(ResultContract::deliverable(schema()).unwrap()),
        ..f.options()
    }
}

#[test]
fn a_native_root_answer_is_a_deliverable_without_a_tool_child_or_git_change() {
    let f = Fixture::new();
    let before = f.git(&["rev-parse", "HEAD"]);
    let branch = f
        .yard
        .task(reply(&f, "valid", r#"{"answer":"supported finding"}"#))
        .options(options(&f))
        .name("research")
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::NoChanges);
    assert!(branch.info().candidate.is_none());
    assert!(branch.info().parent.is_none());
    assert!(branch.info().children.is_empty());
    let events = branch.events().unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.activity, Activity::Prompt(_)))
            .count(),
        1
    );
    assert_eq!(
        branchyard::contract::current(&events),
        Some(json!({"answer":"supported finding"}))
    );
    assert_eq!(f.git(&["rev-parse", "HEAD"]), before);
    let attempts = f.yard.compare(&["research".into()], true).unwrap();
    assert!(attempts[0].is_deliverable());
    assert_eq!(
        attempts[0].result,
        Some(json!({"answer":"supported finding"}))
    );
    assert!(attempts[0].result_event.is_some());
    let judged = f
        .yard
        .judge(&["research".into()], &JudgeOptions::default())
        .unwrap();
    assert_eq!(judged.pick.as_deref(), Some("research"));
    assert!(judged.candidates[0]
        .reason
        .contains("semantic accuracy not verified"));
    let outcome = f
        .yard
        .outcomes(None)
        .unwrap()
        .into_iter()
        .find(|o| o.branch == "research")
        .unwrap();
    assert_eq!(outcome.outcome, BranchOutcome::Ready);
}

#[test]
fn root_report_metadata_does_not_turn_unchanged_code_into_a_deliverable() {
    let f = Fixture::new();
    let branch = f
        .task(&reply(&f, "metadata", r#"{"answer":"metadata"}"#))
        .result(ResultContract::new(schema(), true).unwrap())
        .name("code")
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::NoChanges);
    assert!(branchyard::contract::current(&branch.events().unwrap()).is_some());
    let judged = f
        .yard
        .judge(&["code".into()], &JudgeOptions::default())
        .unwrap();
    assert_eq!(judged.pick, None);
    assert!(!judged.candidates[0].eligible);
    assert_eq!(
        f.yard.outcomes(None).unwrap()[0].outcome,
        BranchOutcome::Failed
    );
}

#[test]
fn invalid_and_previous_turn_outputs_are_incomplete_without_automatic_retries() {
    let f = Fixture::new();
    for (name, body) in [("plain", "a prose answer"), ("invalid", r#"{"answer":42}"#)] {
        let branch = f
            .yard
            .task(reply(&f, name, body))
            .options(options(&f))
            .name(name)
            .run()
            .unwrap();
        assert_eq!(branch.info().turns, 1);
        assert!(branch.info().children.is_empty());
        let judged = f
            .yard
            .judge(&[name.into()], &JudgeOptions::default())
            .unwrap();
        assert_eq!(judged.pick, None);
        assert!(judged.candidates[0].attempt.result_error.is_some());
    }
    let branch = f
        .yard
        .task(reply(&f, "first", r#"{"answer":"first turn"}"#))
        .options(options(&f))
        .name("stale")
        .run()
        .unwrap();
    let branch = branch.send("say hi", f.options()).unwrap();
    assert_eq!(
        branch.info().turns,
        2,
        "only the explicitly requested second turn runs"
    );
    let events = branch.events().unwrap();
    assert_eq!(
        branchyard::contract::latest(&events),
        Some(json!({"answer":"first turn"}))
    );
    assert_eq!(branchyard::contract::current(&events), None);
    let judged = f
        .yard
        .judge(&["stale".into()], &JudgeOptions::default())
        .unwrap();
    assert_eq!(judged.pick, None);
    assert!(judged.candidates[0].reason.contains("latest turn"));
    let outcome = f
        .yard
        .outcomes(None)
        .unwrap()
        .into_iter()
        .find(|o| o.branch == "stale")
        .unwrap();
    assert_eq!(outcome.outcome, BranchOutcome::Failed);
}

struct FixedJudge(bool);
impl Judge for FixedJudge {
    fn name(&self) -> String {
        "fake verdict".into()
    }
    fn verdict(
        &self,
        _: &branchyard::Yard,
        prompt: &str,
        names: &[String],
    ) -> Result<String, Error> {
        assert!(prompt.contains("structured result"));
        assert!(prompt.contains("not truth"));
        if !self.0 {
            return Ok("not a verdict".into());
        }
        Ok(json!({"ranking":names, "scores":names.iter().map(|n|(n.clone(), json!(100))).collect::<serde_json::Map<_,_>>(),
            "reasons":names.iter().map(|n|(n.clone(), json!("claimed best"))).collect::<serde_json::Map<_,_>>()}).to_string())
    }
}

#[test]
fn model_scores_cannot_bypass_contracts_and_required_invalid_verdicts_fail_closed() {
    let f = Fixture::new();
    f.yard
        .task(reply(&f, "bad", "not json"))
        .options(options(&f))
        .name("bad")
        .run()
        .unwrap();
    f.yard
        .task(reply(&f, "good", r#"{"answer":"evidence"}"#))
        .options(options(&f))
        .name("good")
        .run()
        .unwrap();
    let names = vec!["bad".into(), "good".into()];
    let judged = f
        .yard
        .judge(
            &names,
            &JudgeOptions {
                judge: Some(Arc::new(FixedJudge(true))),
                require_judge: true,
                ..JudgeOptions::default()
            },
        )
        .unwrap();
    assert_eq!(judged.pick.as_deref(), Some("good"));
    assert!(
        !judged.candidates[0].eligible,
        "a score of 100 cannot invent a valid result"
    );
    let judged = f
        .yard
        .judge(
            &names,
            &JudgeOptions {
                judge: Some(Arc::new(FixedJudge(false))),
                require_judge: true,
                ..JudgeOptions::default()
            },
        )
        .unwrap();
    assert!(matches!(judged.by, JudgedBy::RequiredFailed { .. }));
    assert_eq!(judged.pick, None);
    assert!(judged.candidates.iter().all(|c| !c.eligible));
    assert!(f
        .yard
        .judge(
            &names,
            &JudgeOptions {
                require_judge: true,
                ..JudgeOptions::default()
            }
        )
        .is_err());
}

#[test]
fn both_fan_paths_capture_native_results_without_a_git_candidate() {
    let f = Fixture::new();
    let prompt = reply(
        &f,
        "fan",
        r#"Native planning and progress can precede the answer.
```json
{"answer":"parallel finding"}
```"#,
    );
    let mut task = options(&f);
    task.name = Some("explicit".into());
    let branches = f
        .yard
        .task(&prompt)
        .options(task.clone())
        .run_on(&["gemini-cli", "qwen-code"])
        .unwrap();
    let names: Vec<String> = branches.iter().map(|b| b.info().name.clone()).collect();
    let compared = f.yard.compare(&names, false).unwrap();
    assert_eq!(compared.len(), 2);
    assert!(compared
        .iter()
        .all(|a| a.candidate.is_none() && a.result.is_some()));
    let table = Fleet {
        entries: [(
            "default".into(),
            FleetEntry {
                candidates: vec![FleetCandidate::new("gemini-cli")],
                attempts: 2,
                ..FleetEntry::default()
            },
        )]
        .into(),
    };
    task.name = Some("routed".into());
    let routed = f
        .yard
        .fan_routed(
            &prompt,
            &task,
            &table,
            &RouteOptions {
                seed: Some(1),
                ..RouteOptions::default()
            },
        )
        .unwrap();
    let names: Vec<String> = routed
        .branches
        .iter()
        .map(|b| b.info().name.clone())
        .collect();
    assert_eq!(names.len(), 2);
    assert!(f
        .yard
        .compare(&names, false)
        .unwrap()
        .iter()
        .all(|a| a.result.is_some()));
    assert!(f
        .yard
        .judge(&names, &JudgeOptions::default())
        .unwrap()
        .pick
        .is_some());
}

#[test]
fn deliverable_goal_and_invalid_contracts_are_refused_before_creating_a_branch() {
    let f = Fixture::new();
    let mut task = options(&f);
    task.goal = Some(Goal::new("finished research"));
    assert!(f
        .yard
        .task("say hi")
        .options(task)
        .name("goal")
        .run()
        .is_err());
    assert!(f.yard.branch("goal").is_err());
    let mut contract = ResultContract::deliverable(schema()).unwrap();
    contract.required = false;
    assert_eq!(contract.purpose, ResultPurpose::Deliverable);
    let mut task = f.options();
    task.result = Some(contract);
    assert!(f
        .yard
        .task("say hi")
        .options(task)
        .name("invalid")
        .run()
        .is_err());
    assert!(f.yard.branch("invalid").is_err());
}

/// Existing opt-in routing failover must carry the output contract forward.
#[test]
fn a_routed_harness_fault_keeps_the_deliverable_contract_on_its_replacement() {
    let f = Fixture::new();
    let prompt = reply(&f, "replacement", r#"{"answer":"replacement evidence"}"#);
    let mut broken = FleetCandidate::new("gemini-cli");
    broken.command = Some(vec!["/bin/sh".into(), "-c".into(), "exit 1".into()]);
    let table = Fleet {
        entries: [(
            "default".into(),
            FleetEntry {
                candidates: vec![broken, FleetCandidate::new("qwen-code")],
                failover: true,
                ..FleetEntry::default()
            },
        )]
        .into(),
    };
    let mut task = options(&f);
    task.name = Some("replacement".into());
    let seed = (0..100)
        .find(|seed| {
            f.yard
                .route(
                    &prompt,
                    &task,
                    &table,
                    &RouteOptions {
                        seed: Some(*seed),
                        ..RouteOptions::default()
                    },
                    Some(1),
                )
                .unwrap()
                .picks[0]
                .index
                == 0
        })
        .unwrap();
    let routed = f
        .yard
        .run_routed(
            &prompt,
            &task,
            &table,
            &RouteOptions {
                seed: Some(seed),
                ..RouteOptions::default()
            },
        )
        .unwrap();
    assert_eq!(routed.failovers.len(), 1);
    let names: Vec<String> = routed
        .branches
        .iter()
        .map(|branch| branch.info().name.clone())
        .collect();
    let compared = f.yard.compare(&names, false).unwrap();
    assert!(compared
        .iter()
        .all(|attempt| attempt.is_deliverable() && attempt.result.is_some()));
    assert!(f
        .yard
        .judge(&names, &JudgeOptions::default())
        .unwrap()
        .pick
        .is_some());
}

/// A schema-valid answer cannot bypass the caller's configured tree check.
#[test]
fn unchanged_deliverables_honor_configured_checks_on_the_exact_base() {
    let f = Fixture::new();
    let prompt = reply(&f, "checked", r#"{"answer":"valid structure"}"#);
    let mut failing = options(&f);
    failing.check = Some(vec!["/bin/sh".into(), "-c".into(), "exit 23".into()]);
    let branch = f
        .yard
        .task(&prompt)
        .options(failing)
        .name("check-fails")
        .run()
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::NoChanges);
    let names = vec!["check-fails".into()];
    let skipped = f.yard.judge(&names, &JudgeOptions::default()).unwrap();
    assert_eq!(
        skipped.pick, None,
        "a configured check may not be silently skipped"
    );
    assert!(matches!(
        skipped.candidates[0].attempt.check,
        branchyard::AttemptCheck::NotRun
    ));
    let checked = f
        .yard
        .judge(
            &names,
            &JudgeOptions {
                run_checks: true,
                ..JudgeOptions::default()
            },
        )
        .unwrap();
    assert_eq!(checked.pick, None);
    assert!(matches!(
        checked.candidates[0].attempt.check,
        branchyard::AttemptCheck::Failed { .. }
    ));
    assert!(
        checked.candidates[0].attempt.candidate.is_none(),
        "never fabricate a code candidate"
    );
    let mut passing = options(&f);
    passing.check = Some(vec![
        "git".into(),
        "diff".into(),
        "--quiet".into(),
        "HEAD".into(),
    ]);
    f.yard
        .task(&prompt)
        .options(passing)
        .name("check-passes")
        .run()
        .unwrap();
    let checked = f
        .yard
        .judge(
            &["check-passes".into()],
            &JudgeOptions {
                run_checks: true,
                ..JudgeOptions::default()
            },
        )
        .unwrap();
    assert_eq!(checked.pick.as_deref(), Some("check-passes"));
    assert!(matches!(
        checked.candidates[0].attempt.check,
        branchyard::AttemptCheck::Passed
    ));
    assert!(checked.candidates[0].attempt.candidate.is_none());
}

#[test]
fn a_read_only_judge_never_inherits_the_workers_unrouted_tool_bypass() {
    let f = Fixture::new();
    f.yard
        .task(reply(&f, "answer", r#"{"answer":"finding"}"#))
        .options(options(&f))
        .name("answer")
        .run()
        .unwrap();
    let mut inherited = f.options();
    inherited.unapproved_tools = true;
    let spec = JudgeSpec {
        harness: "antigravity-stream-json".into(),
        command: Some(vec!["/nonexistent/must-not-start".into()]),
        model: None,
        effort: None,
        rubric: None,
    };
    let judge = branchyard::harness_judge(&spec, &inherited);
    let required = f
        .yard
        .judge(
            &["answer".into()],
            &JudgeOptions {
                judge: Some(judge.clone()),
                require_judge: true,
                ..JudgeOptions::default()
            },
        )
        .unwrap();
    assert!(
        matches!(&required.by, JudgedBy::RequiredFailed { error, .. }
        if error.contains("does not route tool permission requests")),
        "{:?}",
        required.by
    );
    assert!(required.pick.is_none());
    assert!(required
        .candidates
        .iter()
        .all(|candidate| !candidate.eligible));
    let optional = f
        .yard
        .judge(
            &["answer".into()],
            &JudgeOptions {
                judge: Some(judge),
                ..JudgeOptions::default()
            },
        )
        .unwrap();
    assert!(
        matches!(&optional.by, JudgedBy::Fallback { error, .. }
        if error.contains("does not route tool permission requests")),
        "{:?}",
        optional.by
    );
    assert_eq!(optional.pick.as_deref(), Some("answer"));
    assert_eq!(f.yard.branches().unwrap().len(), 1);
}
