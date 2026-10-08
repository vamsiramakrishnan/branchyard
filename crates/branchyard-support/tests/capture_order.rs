//! `testing::capture` sees an event whose callsite another thread reached
//! first with no subscriber. tracing caches each callsite's interest for
//! the whole process when the callsite is first reached, so this is its own
//! test binary, with one test: here the callsite is fresh and the order is
//! forced. `a_panic_while_holding_a_lock_logs_one_warning_naming_it` failed
//! a gate run this way ("logged once, not per lock: []").

#![allow(clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::sync::{Arc, Mutex};

use branchyard_support::testing::capture;
use branchyard_support::LockExt;

fn poison<T: Send + 'static>(lock: &Arc<Mutex<T>>) {
    let lock = lock.clone();
    let result = std::thread::spawn(move || {
        let _held = lock.lock().unwrap();
        panic!("poisoning on purpose");
    })
    .join();
    assert!(result.is_err());
}

#[test]
fn a_warning_another_thread_reached_first_is_still_captured() {
    let elsewhere = Arc::new(Mutex::new(1));
    poison(&elsewhere);
    let lock = Arc::new(Mutex::new(7));
    poison(&lock);
    let (_, events) = capture(|| {
        // Another thread, with no subscriber of its own, reaches the
        // warning first, while this capture is installed.
        std::thread::spawn(move || drop(elsewhere.lock_recovering("elsewhere")))
            .join()
            .unwrap();
        drop(lock.lock_recovering("fixture counter"));
    });
    let warnings: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN)
        .collect();
    assert_eq!(warnings.len(), 1, "{events:?}");
    assert!(warnings[0].text.contains("fixture counter"), "{events:?}");
}
