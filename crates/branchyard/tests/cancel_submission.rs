//! Native cancellation at the last prompt boundary, using only the fake ACP agent.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use branchyard::{Activity, BranchStatus, Event, TaskOptions};

#[test]
fn cancellation_from_the_prompt_observer_never_submits_the_prompt_to_the_harness() {
    let fixture = common::Fixture::new();
    let yard = fixture.yard.clone();
    let seen = Arc::new(AtomicBool::new(false));
    let observed = seen.clone();
    let branch = fixture
        .yard
        .task("WRITE must-not-exist.txt=not-submitted")
        .options(TaskOptions {
            name: Some("cancel-before-submit".into()),
            observer: Some(Arc::new(move |event| {
                if matches!(event.activity, Activity::Prompt(_)) {
                    observed.store(true, Ordering::SeqCst);
                    yard.cancel_as(&event.branch, "the prompt-boundary regression")
                        .unwrap();
                }
            })),
            ..fixture.options()
        })
        .run()
        .unwrap();
    assert!(seen.load(Ordering::SeqCst));
    assert_eq!(branch.info().status, BranchStatus::Interrupted);
    assert!(!branch.info().worktree.join("must-not-exist.txt").exists());
    assert!(!branch.events().unwrap().iter().any(|event| {
        matches!(&event.activity, Activity::Harness(Event::MessageDelta { text, .. }) if text.contains("wrote must-not-exist.txt"))
    }));
}
