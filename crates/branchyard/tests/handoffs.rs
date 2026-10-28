//! Cooperative handoffs use fake native sessions and immutable local artifacts.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::collections::BTreeMap;
use std::fs;

use branchyard::{
    Activity, Branch, Delegate, Envelope, Error, HandoffRequest, Policy, ResultContract, Spawn,
    TaskOptions,
};
use branchyard_testkit::wait;
use common::{fake_agent, Fixture};
use serde_json::json;

fn team(f: &Fixture) -> (Branch, Delegate, TaskOptions) {
    let options = TaskOptions {
        delegation: Some(Envelope::depth(2)),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        ..f.options()
    };
    let branch = f
        .yard
        .task("coordinate")
        .options(options.clone())
        .name("lead")
        .run()
        .unwrap();
    let lead = branch.delegate(options.clone()).unwrap();
    (branch, lead, options)
}

fn coworker(lead: &Delegate, name: &str, prompt: &str) {
    lead.spawn(Spawn {
        name: Some(name.into()),
        prompt: prompt.into(),
        result: Some(ResultContract::new(json!({"type": "object"}), true).unwrap()),
        ..Spawn::default()
    })
    .unwrap();
}

fn handoff(from: &str, to: &str) -> HandoffRequest {
    HandoffRequest {
        from: from.into(),
        to: to.into(),
        ..HandoffRequest::default()
    }
}

fn denied(result: Result<impl std::fmt::Debug, Error>, needle: &str) {
    match result {
        Err(Error::Denied(reason)) if reason.contains(needle) => {}
        other => panic!("expected denial mentioning {needle:?}, got {other:?}"),
    }
}

#[test]
fn a_handoff_freezes_results_and_grants_only_selected_artifacts_without_steering() {
    let f = Fixture::new();
    let (root, lead, options) = team(&f);
    coworker(&lead, "research", "collect evidence");
    root.wait_subtree().unwrap();
    let research = f
        .yard
        .branch("research")
        .unwrap()
        .delegate(options.clone())
        .unwrap();
    let value = json!({"summary": "bounded retries", "sources": ["fixture"]});
    research.report_result("evidence ready", &value).unwrap();
    let payload = f.dir.join("evidence.txt");
    fs::write(&payload, b"immutable evidence").unwrap();
    let artifact = research
        .publish_artifact(&payload, None, None, BTreeMap::new())
        .unwrap();
    let hidden = research
        .publish_artifact(&payload, Some("unselected".into()), None, BTreeMap::new())
        .unwrap();
    coworker(&lead, "writer", "HANG");
    let writer = f.yard.branch("writer").unwrap().delegate(options).unwrap();
    denied(
        writer.read_artifact(&artifact.id, &f.dir.join("before.txt")),
        "may not read",
    );
    wait::until("writer submits its native turn", || {
        f.yard
            .branch("writer")
            .unwrap()
            .events()
            .unwrap()
            .iter()
            .any(|event| matches!(event.activity, Activity::Prompt(_)))
    });
    let before = f.yard.branch("writer").unwrap().events().unwrap();
    let request = HandoffRequest {
        artifacts: vec![artifact.id.clone()],
        note: "Keep claims tied to sources".into(),
        ..handoff("research", "writer")
    };
    let sent = lead.handoff(request.clone()).unwrap();
    assert!(sent.next_turn);
    assert!(!sent.delivered);
    let snapshot = sent.handoff.as_ref().unwrap();
    assert_eq!(snapshot.result, value);
    assert_eq!(snapshot.source, "research");
    assert_eq!(snapshot.version.turn, 1);
    assert!(snapshot.version.result_event > 0);
    assert_eq!(
        snapshot.artifacts.as_slice(),
        std::slice::from_ref(&artifact)
    );
    assert_eq!(lead.handoff(request).unwrap().id, sent.id);
    assert_eq!(writer.inbox().unwrap().messages.len(), 1);
    writer
        .read_artifact(&artifact.id, &f.dir.join("after.txt"))
        .unwrap();
    assert_eq!(
        fs::read(f.dir.join("after.txt")).unwrap(),
        b"immutable evidence"
    );
    denied(
        writer.read_artifact(&hidden.id, &f.dir.join("hidden.txt")),
        "may not read",
    );
    let after = f.yard.branch("writer").unwrap().events().unwrap();
    assert_eq!(
        after
            .iter()
            .filter(|e| matches!(e.activity, Activity::Prompt(_)))
            .count(),
        before
            .iter()
            .filter(|e| matches!(e.activity, Activity::Prompt(_)))
            .count()
    );
    assert!(!after
        .iter()
        .any(|e| matches!(e.activity, Activity::Steered { .. })));
    research
        .report_result("updated evidence", &json!({"summary": "newer result"}))
        .unwrap();
    assert_eq!(
        writer.inbox().unwrap().messages[0]
            .handoff
            .as_ref()
            .unwrap()
            .result,
        value
    );
    lead.cancel("writer").unwrap();
    root.wait_subtree().unwrap();
    lead.send("writer", "Write using the pending handoff")
        .unwrap();
    root.wait_subtree().unwrap();
    let events = f.yard.branch("writer").unwrap().events().unwrap();
    assert!(
        events.iter().any(|e| matches!(&e.activity,
        Activity::Prompt(text) if text.contains("bounded retries") && text.contains(&artifact.id)))
    );
    assert!(writer.inbox().unwrap().messages[0].delivered);
}

#[test]
fn handoffs_refuse_missing_stale_or_replaced_versions_and_unrelated_leads() {
    let f = Fixture::new();
    let (root, lead, options) = team(&f);
    coworker(&lead, "source", "research");
    coworker(&lead, "target", "prepare outline");
    root.wait_subtree().unwrap();
    denied(
        lead.handoff(handoff("source", "target")),
        "no accepted result",
    );
    let source = f
        .yard
        .branch("source")
        .unwrap()
        .delegate(options.clone())
        .unwrap();
    source.report_result("done", &json!({"answer": 1})).unwrap();
    let first = lead.handoff(handoff("source", "target")).unwrap();
    let version = first.handoff.unwrap().version;
    let foreign = f
        .yard
        .task("unrelated")
        .options(options.clone())
        .name("foreign")
        .run()
        .unwrap();
    denied(
        foreign
            .delegate(options.clone())
            .unwrap()
            .handoff(handoff("source", "target")),
        "common ancestor",
    );
    lead.send("source", "next research turn").unwrap();
    root.wait_subtree().unwrap();
    denied(
        lead.handoff(handoff("source", "target")),
        "no accepted result",
    );
    source
        .report_result("new result", &json!({"answer": 2}))
        .unwrap();
    denied(
        lead.handoff(HandoffRequest {
            expected: Some(version),
            ..handoff("source", "target")
        }),
        "expected version",
    );
    let next = f.yard.handoff(handoff("source", "target")).unwrap();
    assert_eq!(next.handoff.unwrap().result, json!({"answer": 2}));
    let repeated = f.yard.handoff(handoff("source", "target")).unwrap();
    assert_eq!(repeated.id, next.id);
}

#[test]
fn replacing_a_recipient_does_not_inherit_its_handoff_or_artifact_share() {
    let f = Fixture::new();
    let (root, lead, options) = team(&f);
    coworker(&lead, "source", "research");
    coworker(&lead, "target", "outline");
    root.wait_subtree().unwrap();
    let source = f
        .yard
        .branch("source")
        .unwrap()
        .delegate(options.clone())
        .unwrap();
    source.report_result("done", &json!({"answer": 1})).unwrap();
    let file = f.dir.join("evidence.txt");
    fs::write(&file, "evidence").unwrap();
    let artifact = source
        .publish_artifact(&file, None, None, BTreeMap::new())
        .unwrap();
    lead.handoff(HandoffRequest {
        artifacts: vec![artifact.id.clone()],
        ..handoff("source", "target")
    })
    .unwrap();
    f.yard.remove("target").unwrap();
    coworker(&lead, "target", "different coworker");
    root.wait_subtree().unwrap();
    let target = f.yard.branch("target").unwrap().delegate(options).unwrap();
    assert!(target.inbox().unwrap().messages.is_empty());
    denied(
        target.read_artifact(&artifact.id, &f.dir.join("leaked.txt")),
        "may not read",
    );
}

#[test]
fn an_invalid_later_native_deliverable_exposes_no_earlier_result() {
    let f = Fixture::new();
    let (root, lead, _) = team(&f);
    let answer = f.dir.join("answer.json");
    fs::write(&answer, r#"{"answer":1}"#).unwrap();
    lead.spawn(Spawn {
        name: Some("native".into()),
        prompt: format!("REPLY_FILE {}", answer.display()),
        result: Some(ResultContract::deliverable(json!({"type":"object"})).unwrap()),
        ..Spawn::default()
    })
    .unwrap();
    coworker(&lead, "consumer", "prepare outline");
    root.wait_subtree().unwrap();
    assert_eq!(
        lead.inspect("native").unwrap().result,
        Some(json!({"answer":1}))
    );
    fs::write(&answer, "this is prose, not contracted JSON").unwrap();
    lead.send("native", &format!("REPLY_FILE {}", answer.display()))
        .unwrap();
    root.wait_subtree().unwrap();
    assert_eq!(lead.inspect("native").unwrap().result, None);
    denied(
        lead.handoff(handoff("native", "consumer")),
        "no accepted result",
    );
}
