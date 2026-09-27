//! Harness-to-harness messages against the fake ACP agent: authority
//! follows the delegation tree, `ask --wait` answers across the store's
//! waits, and pending messages are delivered at a branch's next turn.

mod common;

use std::time::Duration;

use branchyard::{
    Activity, BranchStatus, Budget, DeliveredVia, Envelope, Error, Policy, Spawn, TaskOptions,
};
use common::{fake_agent, text, Fixture};

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
/// for a later one, as `begin_submit` would otherwise be free to
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
    // kid's HANG turn is running and the fake agent takes steering, so the
    // answer was steered straight into it, not left for its next turn.
    assert!(inbox.messages[0].delivered, "steered into the running turn");
    // The engine records the delivery event just after it settles the
    // steer that marked the message delivered; wait for the entry.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let kid_log = f.yard.branch("kid").unwrap().events().unwrap();
        if kid_log.iter().any(|e| matches!(&e.activity,
            Activity::MessagesDelivered { ids, via: DeliveredVia::Steer { .. } } if ids == &[answer.id]))
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{kid_log:?}");
        std::thread::sleep(Duration::from_millis(20));
    }

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
    // root had no running turn when they were sent: delivered at the start.
    assert!(events.iter().any(|e| matches!(&e.activity,
        Activity::MessagesDelivered { via: DeliveredVia::TurnStart, ids } if ids.len() == 3)));

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

/// Wait up to 30 seconds for `done`.
fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn events(f: &Fixture, branch: &str) -> Vec<branchyard::RecordedEvent> {
    f.yard.branch(branch).unwrap().events().unwrap()
}

fn prompts(events: &[branchyard::RecordedEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::Prompt(text) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn delivered(events: &[branchyard::RecordedEvent]) -> Vec<(Vec<u64>, DeliveredVia)> {
    events
        .iter()
        .filter_map(|e| match &e.activity {
            Activity::MessagesDelivered { ids, via } => Some((ids.clone(), via.clone())),
            _ => None,
        })
        .collect()
}

/// `root` has finished one turn and has a finished child `kid`; returns
/// kid's delegate.
fn parent_and_child(f: &Fixture, options: &TaskOptions) -> branchyard::Delegate {
    let root = f
        .yard
        .task("say hi")
        .options(options.clone())
        .name("root")
        .run()
        .unwrap();
    root.delegate(options.clone())
        .unwrap()
        .spawn(spawn("say hi", "kid"))
        .unwrap();
    wait_until("kid's turn to end", || {
        f.yard.branch("kid").unwrap().info().status != BranchStatus::Running
    });
    f.yard
        .branch("kid")
        .unwrap()
        .delegate(options.clone())
        .unwrap()
}

#[test]
fn a_message_is_steered_into_the_parents_running_turn_and_never_repeated() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let kid = parent_and_child(&f, &options);

    // root's next turn waits for steering, as a busy parent would.
    let root = f.yard.branch("root").unwrap();
    let turn = {
        let options = options.clone();
        std::thread::spawn(move || root.send("AWAIT_STEER", options))
    };
    wait_until("root to wait for steering", || {
        text(&events(&f, "root")).contains("waiting for steering")
    });

    let report = kid.report("tests pass").unwrap();
    // The sender waited until the running turn took it.
    let inbox = f
        .yard
        .branch("root")
        .unwrap()
        .delegate(options.clone())
        .unwrap()
        .inbox()
        .unwrap();
    assert!(inbox.messages.iter().all(|m| m.delivered), "{inbox:?}");

    let root = turn.join().unwrap().unwrap();
    assert_eq!(root.info().status, BranchStatus::NoChanges);
    let log = events(&f, "root");
    let said = text(&log);
    assert!(said.contains("steered: <branchyard-inbox>"), "{said}");
    assert!(
        said.contains(&format!("[#{}] report from kid: tests pass", report.id)),
        "{said}"
    );
    assert!(log.iter().any(|e| matches!(&e.activity,
        Activity::Steered { by, .. } if by == "kid")));
    let paths = delivered(&log);
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert_eq!(paths[0].0, [report.id]);
    assert!(
        matches!(paths[0].1, DeliveredVia::Steer { .. }),
        "{paths:?}"
    );

    // The next turn is not given it again.
    let root = root.send("WHOAMI", options.clone()).unwrap();
    let log = root.events().unwrap();
    let last = prompts(&log).pop().unwrap();
    assert!(!last.contains("<branchyard-inbox>"), "{last}");
    assert_eq!(delivered(&log).len(), 1, "no second delivery");
}

#[test]
fn a_message_waits_for_the_next_turn_when_the_parent_cannot_steer() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let kid = parent_and_child(&f, &options);

    // The same agent, without the steering extension.
    let no_steer = TaskOptions {
        command: Some(vec![
            "env".into(),
            "FAKE_ACP_NO_STEER=1".into(),
            fake_agent().display().to_string(),
        ]),
        ..options.clone()
    };
    let root = f.yard.branch("root").unwrap();
    let turn = std::thread::spawn(move || root.send("HANG", no_steer));
    wait_until("root's turn to run", || {
        prompts(&events(&f, "root")).len() == 2
    });

    let question = kid.ask("rename the module?", None).unwrap().message;
    let root_inbox = || {
        f.yard
            .branch("root")
            .unwrap()
            .delegate(options.clone())
            .unwrap()
            .inbox()
            .unwrap()
    };
    assert!(!root_inbox().messages[0].delivered, "left pending");
    // Its steered input was refused, and the turn goes on.
    wait_until("the steer to be refused", || {
        events(&f, "root").iter().any(|e| {
            matches!(&e.activity,
            Activity::Warning(w) if w.contains("was not delivered"))
        })
    });
    assert_eq!(
        f.yard.branch("root").unwrap().info().status,
        BranchStatus::Running
    );
    assert!(!f.yard.cancel("root").unwrap().is_empty());
    turn.join().unwrap().unwrap();
    assert!(!root_inbox().messages[0].delivered, "still pending");

    // Delivered once, at the next turn's start.
    let root = f
        .yard
        .branch("root")
        .unwrap()
        .send("WHOAMI", options.clone())
        .unwrap();
    let log = root.events().unwrap();
    let last = prompts(&log).pop().unwrap();
    assert!(
        last.contains(&format!("[#{}] question from kid", question.id)),
        "{last}"
    );
    assert_eq!(
        delivered(&log),
        [(vec![question.id], DeliveredVia::TurnStart)]
    );
    assert!(root_inbox().messages[0].delivered);
    let root = root.send("WHOAMI", options).unwrap();
    let last = prompts(&root.events().unwrap()).pop().unwrap();
    assert!(!last.contains("<branchyard-inbox>"), "{last}");
}

#[test]
fn a_turn_waiting_for_an_answer_is_not_stalled() {
    let f = Fixture::new();
    let options = delegating(&f, Envelope::depth(2));
    let kid = parent_and_child(&f, &options);
    // kid's next turn hangs, with a short stall window.
    let stalling = TaskOptions {
        budget: Budget::default().stall_after(Duration::from_millis(400)),
        ..options.clone()
    };
    let branch = f.yard.branch("kid").unwrap();
    let turn = std::thread::spawn(move || branch.send("HANG", stalling));
    wait_until("kid's turn to run", || {
        prompts(&events(&f, "kid")).len() == 2
    });
    // The harness blocks in `ask --wait`: the same call a harness's `by ask
    // --wait` reaches through the broker, recorded in the store.
    let asking = std::thread::spawn(move || kid.ask("which way?", Some(Duration::from_secs(2))));
    std::thread::sleep(Duration::from_millis(1500));
    let stalled = |f: &Fixture| {
        events(f, "kid")
            .iter()
            .any(|e| matches!(e.activity, Activity::Stalled { .. }))
    };
    assert!(!stalled(&f), "not stalled while it waits for an answer");
    assert!(!f.yard.branch("kid").unwrap().info().stalled);
    let asked = asking.join().unwrap().unwrap();
    assert!(asked.answer.is_none());
    // Once the wait is over, the turn's idle time counts again.
    wait_until("kid to stall", || stalled(&f));
    assert!(!f.yard.cancel("kid").unwrap().is_empty());
    let kid = turn.join().unwrap().unwrap();
    assert_eq!(kid.info().status, BranchStatus::Interrupted);
}
