//! Harness-to-harness messages against the fake ACP agent: authority
//! follows the delegation tree, `ask --wait` answers across the store's
//! waits, and pending messages are delivered at a branch's next turn.

mod common;

use std::time::Duration;

use branchyard::{Activity, BranchStatus, Envelope, Error, Policy, Spawn, TaskOptions};
use common::{fake_agent, Fixture};

fn delegating(f: &Fixture, envelope: Envelope) -> TaskOptions {
    TaskOptions {
        delegation: Some(envelope),
        delegation_server: Some(vec![fake_agent().display().to_string()]),
        policy: Policy::allow_all(),
        ..f.options()
    }
}

fn spawn(prompt: &str, name: &str) -> Spawn {
    Spawn {
        prompt: prompt.into(),
        name: Some(name.into()),
        ..Spawn::default()
    }
}

/// Wait until `branch`'s own first turn has recorded its `Activity::Prompt`
/// (past delivery at that turn's start), so a message sent to it afterwards
/// is never raced into that same turn's prompt instead of staying pending
/// for a later one, as `deliver_at_turn_start` would otherwise be free to
/// do if the turn's handshake with its harness is still in flight.
fn wait_for_own_prompt(f: &Fixture, branch: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let has_prompt = f
            .yard
            .branch(branch)
            .unwrap()
            .events()
            .unwrap()
            .iter()
            .any(|e| matches!(e.activity, Activity::Prompt(_)));
        if has_prompt {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{branch} never recorded its own prompt"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_child_reports_and_asks_its_parent_across_both_event_logs() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let root = f
        .yard
        .task("WRITE root.txt=r")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let root_delegate = root.delegate(options.clone()).unwrap();
    let kid = root_delegate
        .spawn(spawn("HANG", "kid"))
        .expect("a child within the envelope");
    assert_eq!(kid.depth, 1);
    wait_for_own_prompt(&f, "kid");
    let kid_delegate = f
        .yard
        .branch("kid")
        .unwrap()
        .delegate(options.clone())
        .unwrap();

    let report = kid_delegate.report("tests pass").unwrap();
    assert_eq!(report.from, "kid");
    assert_eq!(report.to, "root");
    assert_eq!(report.kind, branchyard::MessageKind::Report);
    assert!(report.id > 0);

    // Recorded on both event logs, so `by events`/`log` show it either way.
    for branch in ["kid", "root"] {
        let events = f.yard.branch(branch).unwrap().events().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(&e.activity, Activity::Message(m) if m.id == report.id)),
            "{branch}'s log is missing the message"
        );
    }

    let asked = kid_delegate
        .ask("should I rename the module?", None)
        .unwrap();
    assert!(asked.answer.is_none(), "no --wait, no answer expected yet");
    assert!(asked.message.in_reply_to.is_none());

    let answer = root_delegate
        .answer(asked.message.id, "yes, rename it")
        .unwrap();
    assert_eq!(answer.to, "kid");
    assert_eq!(answer.in_reply_to, Some(asked.message.id));

    let inbox = kid_delegate.inbox().unwrap();
    assert_eq!(inbox.branch, "kid");
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].id, answer.id);
    assert!(!inbox.messages[0].delivered, "not yet given to a turn");

    // A fresh question, answered from a background thread while `ask`
    // blocks for it: the wait sees the answer across processes' writes,
    // here the same process's, at once.
    let waiting_kid = f
        .yard
        .branch("kid")
        .unwrap()
        .delegate(options.clone())
        .unwrap();
    let waiter = std::thread::spawn(move || {
        waiting_kid.ask("ready to merge?", Some(Duration::from_secs(10)))
    });
    std::thread::sleep(Duration::from_millis(150));
    let root_inbox = root_delegate.inbox().unwrap();
    let latest = root_inbox
        .messages
        .last()
        .expect("the new question arrived");
    root_delegate.answer(latest.id, "yes").unwrap();
    let asked2 = waiter.join().unwrap().unwrap();
    assert_eq!(
        asked2.answer.map(|a| a.text),
        Some("yes".to_owned()),
        "ask --wait should see the answer sent while it blocked"
    );

    // Delivery: the parent's next turn is given every pending message,
    // acknowledged (marked delivered) in the same step, so it is not
    // delivered twice.
    f.yard
        .branch("root")
        .unwrap()
        .send("WHOAMI", options.clone())
        .unwrap();
    loop {
        if f.yard.branch("root").unwrap().info().status != BranchStatus::Running {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let events = f.yard.branch("root").unwrap().events().unwrap();
    let prompt = events
        .iter()
        .rev()
        .find_map(|e| match &e.activity {
            Activity::Prompt(text) => Some(text.clone()),
            _ => None,
        })
        .expect("a prompt was recorded");
    assert!(prompt.contains("<branchyard-inbox>"), "{prompt}");
    assert!(prompt.ends_with("WHOAMI"), "{prompt}");

    let now_delivered = root_delegate.inbox().unwrap();
    assert!(
        now_delivered.messages.iter().all(|m| m.delivered),
        "{now_delivered:?}"
    );
}

#[test]
fn messaging_authority_follows_the_delegation_tree() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let root = f
        .yard
        .task("WRITE root.txt=r")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    let root_delegate = root.delegate(options.clone()).unwrap();
    root_delegate.spawn(spawn("HANG", "a")).expect("child a");
    root_delegate.spawn(spawn("HANG", "b")).expect("child b");
    wait_for_own_prompt(&f, "a");
    wait_for_own_prompt(&f, "b");

    let a_delegate = f
        .yard
        .branch("a")
        .unwrap()
        .delegate(options.clone())
        .unwrap();
    let b_delegate = f.yard.branch("b").unwrap().delegate(options).unwrap();

    // A branch answers only its own descendants, never a sibling's.
    let question = a_delegate.ask("ok?", None).unwrap().message;
    match b_delegate.answer(question.id, "no") {
        Err(Error::Denied(why)) => assert!(
            why.contains("descendants"),
            "expected an authority refusal, got {why:?}"
        ),
        other => panic!("expected Denied, got {other:?}"),
    }

    // A question or a report may go only to one's own parent, not a sibling
    // or an unrelated branch.
    match a_delegate.answer(question.id, "yes") {
        // a is not an ancestor of a: refused as self-messaging or as not a
        // descendant, either way not honored.
        Err(Error::Denied(_)) => {}
        other => panic!("expected Denied, got {other:?}"),
    }
}
