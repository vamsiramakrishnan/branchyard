//! Routing, failover, outcomes and the judge, against the fake ACP agent.
//! No model is called: a judge harness is the fake agent answering a canned
//! verdict from a file.

mod common;

use std::sync::Arc;

use branchyard::{
    harness_fault, Activity, BranchOutcome, BranchStatus, Error, Fleet, FleetActivity,
    FleetCandidate, FleetEntry, Judge, JudgeOptions, JudgeSpec, JudgedBy, Provisioning,
    RecordedEvent, RouteOptions, TaskKind, TaskOptions, Yard,
};
use common::{fake_agent, Fixture};

fn candidate(harness: &str) -> FleetCandidate {
    FleetCandidate::new(harness)
}

fn fleet(entry: FleetEntry) -> Fleet {
    Fleet {
        entries: [("default".to_owned(), entry)].into_iter().collect(),
    }
}

fn fleet_activity(events: &[RecordedEvent]) -> Vec<FleetActivity> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Fleet(a) => Some(a.as_ref().clone()),
            _ => None,
        })
        .collect()
}

/// A seed under which the router's first pick is candidate `index`.
fn seed_picking(
    yard: &Yard,
    prompt: &str,
    options: &TaskOptions,
    fleet: &Fleet,
    index: usize,
) -> u64 {
    (0..1000)
        .find(|seed| {
            let how = RouteOptions {
                seed: Some(*seed),
                ..RouteOptions::default()
            };
            yard.route(prompt, options, fleet, &how, Some(1))
                .unwrap()
                .picks[0]
                .index
                == index
        })
        .expect("some seed picks it")
}

#[test]
fn a_routed_run_records_its_route_kind_and_outcome() {
    let f = Fixture::new();
    let table = fleet(FleetEntry {
        candidates: vec![candidate("gemini-cli"), candidate("qwen-code")],
        ..FleetEntry::default()
    });
    let prompt = "Fix the typo WRITE fixed.txt=yes";
    let how = RouteOptions {
        seed: Some(7),
        ..RouteOptions::default()
    };
    let planned = f
        .yard
        .route(prompt, &f.options(), &table, &how, None)
        .unwrap();
    assert_eq!(planned.kind, TaskKind::Bugfix);
    assert_eq!(planned.matched, ["fix", "fixed"]);
    // The same seed routes the same way.
    let again = f
        .yard
        .route(prompt, &f.options(), &table, &how, None)
        .unwrap();
    assert_eq!(planned.picks, again.picks);
    let routed = f
        .yard
        .run_routed(prompt, &f.options(), &table, &how)
        .unwrap();
    assert_eq!(routed.branches.len(), 1);
    let branch = &routed.branches[0];
    assert_eq!(branch.info().status, BranchStatus::Ready);
    assert_eq!(branch.info().harness, planned.picks[0].candidate.harness);
    let activity = fleet_activity(&branch.events().unwrap());
    let FleetActivity::Routed(decision) = &activity[0] else {
        panic!("{activity:?}");
    };
    assert_eq!(decision.kind, TaskKind::Bugfix);
    assert_eq!(decision.kind_source, "classifier");
    assert_eq!(decision.entry.as_deref(), Some("default"));
    assert_eq!(decision.fallbacks.len(), 1);
    let outcomes = f.yard.outcomes(None).unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].branch, branch.info().name);
    assert_eq!(outcomes[0].outcome, BranchOutcome::Ready);
    assert_eq!(outcomes[0].kind, TaskKind::Bugfix);
    assert!(outcomes[0].routed);

    // A merge is learned.
    f.yard.merge(&branch.info().name, "main").unwrap();
    let outcomes = f.yard.outcomes(Some(TaskKind::Bugfix)).unwrap();
    assert_eq!(outcomes[0].outcome, BranchOutcome::Merged);

    // A run with a named harness and a kind records the kind only.
    let named = f
        .yard
        .run_with_kind("WRITE b.txt=1", &f.options(), TaskKind::Docs)
        .unwrap();
    let docs = f.yard.outcomes(Some(TaskKind::Docs)).unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].branch, named.info().name);
    assert!(!docs[0].routed);
}

#[test]
fn a_harness_failure_fails_over_to_the_next_candidate() {
    let f = Fixture::new();
    let broken = FleetCandidate {
        command: Some(vec!["/bin/false".into()]),
        ..candidate("gemini-cli")
    };
    let table = fleet(FleetEntry {
        candidates: vec![broken, candidate("qwen-code")],
        failover: true,
        exploration: 0.0,
        ..FleetEntry::default()
    });
    let prompt = "Add a file WRITE added.txt=yes";
    let seed = seed_picking(&f.yard, prompt, &f.options(), &table, 0);
    let how = RouteOptions {
        seed: Some(seed),
        ..RouteOptions::default()
    };
    let routed = f
        .yard
        .run_routed(prompt, &f.options(), &table, &how)
        .unwrap();
    assert_eq!(routed.failovers.len(), 1, "{:?}", routed.failovers);
    let (from, to) = &routed.failovers[0];
    let final_branch = &routed.branches[0];
    assert_eq!(&final_branch.info().name, to);
    assert_eq!(final_branch.info().status, BranchStatus::Ready);
    assert_eq!(final_branch.info().harness, "qwen-code");
    assert_eq!(final_branch.info().parent.as_deref(), Some(from.as_str()));
    // The task went on with its own prompt: the failed branch ran no turn.
    assert_eq!(final_branch.info().prompt, prompt);

    let failed = f.yard.branch(from).unwrap();
    let why = harness_fault(&failed.info().status);
    assert!(why.is_some(), "{:?}", failed.info().status);
    assert_eq!(failed.info().superseded_by.as_deref(), Some(to.as_str()));
    let on_failed = fleet_activity(&failed.events().unwrap());
    assert!(
        on_failed.iter().any(
            |a| matches!(a, FleetActivity::FailedOver { next, .. } if next.harness == "qwen-code")
        ),
        "{on_failed:?}"
    );
    let on_next = fleet_activity(&final_branch.events().unwrap());
    let Some(FleetActivity::Routed(decision)) = on_next.first() else {
        panic!("{on_next:?}");
    };
    assert_eq!(decision.from.as_deref(), Some(from.as_str()));
    assert_eq!(decision.tried.len(), 1);
    assert!(decision.fallbacks.is_empty());

    let outcomes = f.yard.outcomes(None).unwrap();
    let of = |name: &str| outcomes.iter().find(|o| o.branch == name).unwrap().outcome;
    assert_eq!(of(from), BranchOutcome::Failed);
    assert_eq!(of(to), BranchOutcome::Ready);
}

#[test]
fn a_task_failure_or_a_chain_without_candidates_never_fails_over() {
    let f = Fixture::new();
    // goose cannot take a model over ACP: its driver refuses the turn,
    // which is the task's configuration, not the harness failing.
    let modelled = FleetCandidate {
        model: Some("m".into()),
        ..candidate("goose")
    };
    let table = fleet(FleetEntry {
        candidates: vec![modelled, candidate("qwen-code")],
        failover: true,
        exploration: 0.0,
        ..FleetEntry::default()
    });
    let seed = seed_picking(&f.yard, "x", &f.options(), &table, 0);
    let how = RouteOptions {
        seed: Some(seed),
        ..RouteOptions::default()
    };
    let routed = f.yard.run_routed("x", &f.options(), &table, &how).unwrap();
    assert!(routed.failovers.is_empty());
    let branch = &routed.branches[0];
    assert!(
        matches!(&branch.info().status, BranchStatus::Failed { reason } if reason.contains("model selection")),
        "{:?}",
        branch.info().status
    );
    assert_eq!(harness_fault(&branch.info().status), None);

    // The only candidate broken: nothing left to fail over to, said so.
    let broken = FleetCandidate {
        command: Some(vec!["/bin/false".into()]),
        ..candidate("gemini-cli")
    };
    let alone = fleet(FleetEntry {
        candidates: vec![broken],
        failover: true,
        ..FleetEntry::default()
    });
    let routed = f
        .yard
        .run_routed("y", &f.options(), &alone, &RouteOptions::default())
        .unwrap();
    assert!(routed.failovers.is_empty());
    let events = routed.branches[0].events().unwrap();
    assert!(
        events.iter().any(
            |e| matches!(&e.activity, Activity::Warning(w) if w.contains("no candidate left"))
        ),
        "{events:?}"
    );

    // Failover off: the failure stands.
    let off = fleet(FleetEntry {
        candidates: vec![
            FleetCandidate {
                command: Some(vec!["/bin/false".into()]),
                ..candidate("gemini-cli")
            },
            candidate("qwen-code"),
        ],
        failover: false,
        exploration: 0.0,
        ..FleetEntry::default()
    });
    let seed = seed_picking(&f.yard, "z", &f.options(), &off, 0);
    let routed = f
        .yard
        .run_routed(
            "z",
            &f.options(),
            &off,
            &RouteOptions {
                seed: Some(seed),
                ..RouteOptions::default()
            },
        )
        .unwrap();
    assert!(routed.failovers.is_empty());
    assert!(matches!(
        routed.branches[0].info().status,
        BranchStatus::Failed { .. }
    ));
}

#[test]
fn unavailable_candidates_are_never_routed_to() {
    let f = Fixture::new();
    let missing = FleetCandidate {
        command: Some(vec!["branchyard-no-such-harness".into()]),
        ..candidate("gemini-cli")
    };
    let table = fleet(FleetEntry {
        candidates: vec![missing.clone(), candidate("qwen-code")],
        exploration: 1.0,
        attempts: 3,
        ..FleetEntry::default()
    });
    for seed in 0..20 {
        let how = RouteOptions {
            seed: Some(seed),
            ..RouteOptions::default()
        };
        let route = f.yard.route("x", &f.options(), &table, &how, None).unwrap();
        assert!(route
            .picks
            .iter()
            .all(|p| p.candidate.harness == "qwen-code"));
        assert_eq!(route.excluded[0].candidate, missing);
    }
    let none = fleet(FleetEntry {
        candidates: vec![missing],
        ..FleetEntry::default()
    });
    let refused = f
        .yard
        .run_routed("x", &f.options(), &none, &RouteOptions::default());
    assert!(
        matches!(&refused, Err(Error::HarnessUnavailable { reason, .. }) if reason.contains("not found on PATH")),
        "{refused:?}"
    );
    assert!(f.yard.branches().unwrap().is_empty(), "nothing was created");
    let no_entry = Fleet {
        entries: [("docs".to_owned(), FleetEntry::default())]
            .into_iter()
            .collect(),
    };
    assert!(matches!(
        f.yard.route(
            "Fix it",
            &f.options(),
            &no_entry,
            &RouteOptions::default(),
            None
        ),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn a_routed_fan_starts_the_entrys_attempts_named_by_harness() {
    let f = Fixture::new();
    let table = fleet(FleetEntry {
        candidates: vec![candidate("gemini-cli"), candidate("qwen-code")],
        attempts: 3,
        ..FleetEntry::default()
    });
    let options = TaskOptions {
        name: Some("trio".into()),
        ..f.options()
    };
    let how = RouteOptions {
        seed: Some(3),
        kind: Some(TaskKind::Feature),
        ..RouteOptions::default()
    };
    let routed = f
        .yard
        .fan_routed("WRITE f.txt=1", &options, &table, &how)
        .unwrap();
    let mut names: Vec<String> = routed
        .branches
        .iter()
        .map(|b| b.info().name.clone())
        .collect();
    names.sort();
    assert_eq!(names.len(), 3);
    assert!(names.iter().all(|n| n.starts_with("trio-")), "{names:?}");
    assert_eq!(names.iter().filter(|n| n.ends_with("-2")).count(), 1);
    let mut fan = f.yard.fan_branches("trio").unwrap();
    fan.sort();
    assert_eq!(fan, names);
    for branch in &routed.branches {
        let Some(FleetActivity::Routed(d)) =
            fleet_activity(&branch.events().unwrap()).into_iter().next()
        else {
            panic!()
        };
        assert_eq!(
            (d.kind, d.kind_source.as_str(), d.attempts),
            (TaskKind::Feature, "flag", 3)
        );
    }
}

#[test]
fn a_send_keeps_the_branchs_model() {
    let f = Fixture::new();
    let large = Provisioning {
        model: Some("large".into()),
        ..Provisioning::default()
    };
    let options = TaskOptions {
        harness: Some("claude-code-acp".into()),
        isolated: true,
        provision: Some(large),
        ..f.options()
    };
    let branch = f
        .yard
        .run_with_kind("WRITE a.txt=1", &options, TaskKind::Feature)
        .unwrap();
    assert_eq!(branch.info().status, BranchStatus::Ready);
    let switch = TaskOptions {
        harness: None,
        provision: Some(Provisioning {
            model: Some("small".into()),
            ..Provisioning::default()
        }),
        ..f.options()
    };
    let refused = branch.send("WRITE b.txt=2", switch);
    assert!(
        matches!(&refused, Err(Error::Unsupported(why)) if why.contains("cannot switch")),
        "{refused:?}"
    );
    // A send that names no model keeps the branch's.
    let keep = TaskOptions {
        harness: None,
        provision: Some(Provisioning::default()),
        ..TaskOptions::default()
    };
    let sent = branch.send("WRITE c.txt=3", keep).unwrap();
    assert_eq!(sent.info().status, BranchStatus::Ready);
    let row = &f.yard.outcomes(None).unwrap()[0];
    assert_eq!(row.model.as_deref(), Some("large"));
    assert_eq!(row.turns, 2);
}

/// Three attempts: a small one, a big one, and one whose check fails.
fn three_attempts(f: &Fixture) -> Vec<String> {
    let check = Some(vec![
        "sh".to_owned(),
        "-c".into(),
        "test ! -f bad.txt".into(),
    ]);
    let run = |name: &str, prompt: &str| {
        f.yard
            .task(prompt)
            .options(TaskOptions {
                name: Some(name.into()),
                check: check.clone(),
                ..f.options()
            })
            .run()
            .unwrap()
            .info()
            .name
            .clone()
    };
    vec![
        run("big", "WRITE one.txt=1 WRITE two.txt=2 WRITE three.txt=3"),
        run("small", "WRITE one.txt=1"),
        run("broken", "WRITE bad.txt=1"),
    ]
}

#[test]
fn the_deterministic_judge_ranks_checks_then_size_and_records_scores() {
    let f = Fixture::new();
    let names = three_attempts(&f);
    let judgement = f
        .yard
        .judge(
            &names,
            &JudgeOptions {
                run_checks: true,
                record: true,
                ..JudgeOptions::default()
            },
        )
        .unwrap();
    assert_eq!(judgement.by, JudgedBy::Deterministic);
    let order: Vec<&str> = judgement
        .candidates
        .iter()
        .map(|s| s.attempt.branch.as_str())
        .collect();
    assert_eq!(order, ["small", "big", "broken"]);
    assert_eq!(judgement.pick.as_deref(), Some("small"));
    assert!(!judgement.candidates[2].eligible);
    assert_eq!(judgement.candidates[2].score, 0.0);
    assert!(judgement.candidates[0].score > judgement.candidates[1].score);
    let marks = fleet_activity(&f.yard.branch("small").unwrap().events().unwrap());
    assert!(
        matches!(marks.last(), Some(FleetActivity::Judged(m)) if m.picked && m.rank == 1 && m.check == "passed"),
        "{marks:?}"
    );
    let outcomes = f.yard.outcomes(None).unwrap();
    let of = |n: &str| outcomes.iter().find(|o| o.branch == n).unwrap().clone();
    assert_eq!(of("small").outcome, BranchOutcome::JudgedBest);
    assert_eq!(of("big").outcome, BranchOutcome::Ready);
    assert!(of("big").score.is_some());
}

#[test]
fn a_judge_harness_answers_a_verdict_on_a_read_only_scratch_branch() {
    let f = Fixture::new();
    let names = three_attempts(&f);
    let verdict = f.dir.join("verdict.json");
    std::fs::write(
        &verdict,
        r#"{"ranking": ["broken", "big", "small"],
            "scores": {"big": 90, "small": 40, "broken": 95},
            "reasons": {"big": "covers every case", "small": "misses two files", "broken": "bold"}}"#,
    )
    .unwrap();
    let spec = JudgeSpec {
        harness: "gemini-cli".into(),
        model: None,
        effort: None,
        command: Some(vec![fake_agent().display().to_string()]),
        rubric: Some(format!("REPLY_FILE {}", verdict.display())),
    };
    let judge = branchyard::harness_judge(&spec, &f.options());
    let judgement = f
        .yard
        .judge(
            &names,
            &JudgeOptions {
                run_checks: true,
                judge: Some(judge.clone()),
                rubric: spec.rubric.clone(),
                record: true,
            },
        )
        .unwrap();
    assert_eq!(
        judgement.by,
        JudgedBy::Judge {
            name: "harness gemini-cli".into()
        }
    );
    let order: Vec<&str> = judgement
        .candidates
        .iter()
        .map(|s| s.attempt.branch.as_str())
        .collect();
    assert_eq!(order, ["broken", "big", "small"]);
    // The judge ranked a failed check first; it is still not pickable.
    assert_eq!(judgement.pick.as_deref(), Some("big"));
    assert_eq!(judgement.candidates[1].judge, Some(90.0));
    assert!(judgement.candidates[0].reason.contains("not pickable"));
    // The scratch branch is gone and left no outcome.
    let left: Vec<String> = f
        .yard
        .branches()
        .unwrap()
        .into_iter()
        .map(|b| b.name)
        .collect();
    assert_eq!(left.len(), 3, "{left:?}");
    assert!(f
        .yard
        .outcomes(None)
        .unwrap()
        .iter()
        .all(|o| names.contains(&o.branch)));

    // An answer that is not a strict verdict falls back, saying why.
    std::fs::write(&verdict, "I like big best.").unwrap();
    let judgement = f
        .yard
        .judge(
            &names,
            &JudgeOptions {
                run_checks: true,
                judge: Some(judge),
                rubric: spec.rubric.clone(),
                record: false,
            },
        )
        .unwrap();
    assert!(
        matches!(&judgement.by, JudgedBy::Fallback { error, .. } if error.contains("not a JSON object")),
        "{:?}",
        judgement.by
    );
    assert_eq!(judgement.pick.as_deref(), Some("small"));
}

struct Canned(&'static str);

impl Judge for Canned {
    fn name(&self) -> String {
        "canned".into()
    }
    fn verdict(&self, _: &Yard, prompt: &str, candidates: &[String]) -> Result<String, Error> {
        assert!(prompt.contains("## Candidates"));
        assert_eq!(candidates.len(), 2);
        Ok(self.0.into())
    }
}

#[test]
fn a_judge_is_pluggable() {
    let f = Fixture::new();
    let names = three_attempts(&f)[..2].to_vec();
    let judgement = f
        .yard
        .judge(
            &names,
            &JudgeOptions {
                judge: Some(Arc::new(Canned(
                    r#"{"ranking": ["big", "small"], "scores": {"big": 80, "small": 70},
                        "reasons": {"big": "b", "small": "s"}}"#,
                ))),
                ..JudgeOptions::default()
            },
        )
        .unwrap();
    assert_eq!(judgement.pick.as_deref(), Some("big"));
    assert_eq!(
        judgement.by,
        JudgedBy::Judge {
            name: "canned".into()
        }
    );
    assert!(f
        .yard
        .outcomes(None)
        .unwrap()
        .iter()
        .all(|o| o.score.is_none()));
}
