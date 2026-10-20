//! Plans and the shared brief through the SDK against the fake ACP agent:
//! a plan creates its children in one proposal, all or none, and every
//! child is given its parent's brief, including entries added later.

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use branchyard::{
    Activity, BranchStatus, DelegationPlan, Envelope, Error, Policy, Spawn, TaskOptions,
};
use branchyard_testkit::wait;
use common::{fake_agent, Fixture};
use serde_json::json;

fn delegating(f: &Fixture, envelope: Envelope) -> TaskOptions {
    TaskOptions {
        delegation: Some(envelope),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        ..f.options()
    }
}

/// A plan as the `delegate` tool takes it.
fn plan(value: serde_json::Value) -> DelegationPlan {
    serde_json::from_value(value).unwrap()
}

fn said(f: &Fixture, name: &str) -> String {
    common::text(&f.yard.branch(name).unwrap().events().unwrap())
}

#[test]
fn a_plan_creates_its_children_at_once_with_dependencies_and_scopes() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::default());
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options).unwrap();
    let delegated = delegate
        .delegate(plan(json!({
            "brief": "  Use the Token type in src/token.rs.  ",
            "tasks": [
                {"name": "lexer", "prompt": "WRITE src/lexer/a.txt=1", "owns": ["src/lexer"]},
                {"name": "parser", "prompt": "WRITE src/parser/b.txt=2",
                 "owns": ["src/parser"], "depends_on": ["lexer"]},
            ],
        })))
        .unwrap();
    let names: Vec<&str> = delegated.spawned.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["lexer", "parser"], "in plan order");
    assert_eq!(delegated.spawned[0].status, BranchStatus::Running);
    assert_eq!(delegated.spawned[1].depends_on, ["lexer"]);
    assert_eq!(delegated.spawned[1].owns, ["src/parser"]);
    assert_eq!(delegated.dependencies.len(), 1);
    assert_eq!(delegated.brief_entries, 1);
    assert!(
        delegated.next.starts_with("End your turn"),
        "{}",
        delegated.next
    );

    root.wait_subtree().unwrap();
    for name in ["lexer", "parser"] {
        assert_eq!(
            f.yard.branch(name).unwrap().info().status,
            BranchStatus::Ready
        );
    }
    // The brief is the branch's, trimmed, on its log; and the plan is
    // recorded there like every delegation operation.
    assert_eq!(
        delegate.brief(None).unwrap().entries,
        ["Use the Token type in src/token.rs."]
    );
    let log = f.yard.branch("root").unwrap().events().unwrap();
    assert!(log.iter().any(|e| matches!(&e.activity,
        Activity::Delegation { tool, refused: false, .. } if tool == "delegate")));
}

#[test]
fn a_refused_task_refuses_the_whole_plan_and_records_no_brief() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::default());
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options).unwrap();
    let refused = delegate.delegate(plan(json!({
        "brief": "Never recorded.",
        "tasks": [
            {"name": "good", "prompt": "say hi"},
            {"name": "bad", "prompt": "say hi", "owns": ["../outside"]},
        ],
    })));
    assert!(
        matches!(&refused, Err(Error::Denied(why)) if why.contains("inside the repository")),
        "{refused:?}"
    );
    assert!(f.yard.branch("good").is_err(), "nothing was created");
    assert!(delegate.brief(None).unwrap().entries.is_empty());
    assert!(matches!(
        delegate.delegate(DelegationPlan::default()),
        Err(Error::Denied(why)) if why.contains("at least one task")
    ));
    // A brief entry is checked like any other.
    assert!(matches!(delegate.brief(Some("   ")), Err(Error::Denied(_))));
    let long = "x".repeat(branchyard::BRIEF_ENTRY_MAX + 1);
    assert!(matches!(
        delegate.brief(Some(&long)),
        Err(Error::Denied(why)) if why.contains("16 KiB")
    ));
}

#[test]
fn every_child_is_given_the_brief_including_entries_added_later() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::default());
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options).unwrap();
    delegate
        .delegate(plan(json!({
            "brief": "Rule one: add no crates.",
            "tasks": [{"name": "a", "prompt": "SHOW_INSTRUCTIONS"}],
        })))
        .unwrap();
    root.wait_subtree().unwrap();
    let first = said(&f, "a");
    assert!(first.contains("## The shared brief"), "{first}");
    assert!(first.contains("Rule one: add no crates."), "{first}");

    let brief = delegate
        .brief(Some("Rule two: the lexer returns Spanned<Token>."))
        .unwrap();
    assert_eq!(brief.entries.len(), 2);
    assert!(brief.steered.is_empty(), "nothing was running");
    // Its next turn has both, in order; so does a child spawned later.
    delegate.send("a", "SHOW_INSTRUCTIONS").unwrap();
    delegate
        .spawn(Spawn {
            prompt: "SHOW_INSTRUCTIONS".into(),
            name: Some("b".into()),
            ..Spawn::default()
        })
        .unwrap();
    root.wait_subtree().unwrap();
    for name in ["a", "b"] {
        let text = said(&f, name);
        let one = text.rfind("Rule one").unwrap();
        let two = text.rfind("Rule two").unwrap();
        assert!(one < two, "{name}: {text}");
    }
}

#[test]
fn a_running_child_is_sent_a_new_brief_entry() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::default());
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let delegate = root.delegate(options).unwrap();
    delegate
        .spawn(Spawn {
            prompt: "AWAIT_STEER".into(),
            name: Some("kid".into()),
            ..Spawn::default()
        })
        .unwrap();
    wait::until("the child to wait for steering", || {
        delegate
            .inspect("kid")
            .is_ok_and(|i| i.last_message.contains("waiting for steering"))
    });
    let brief = delegate.brief(Some("Decision: keep the old API.")).unwrap();
    assert_eq!(brief.steered, ["kid"]);
    root.wait_subtree().unwrap();
    let text = said(&f, "kid");
    assert!(
        text.contains("steered: Your parent added to the brief"),
        "{text}"
    );
    assert!(text.contains("Decision: keep the old API."), "{text}");
}
