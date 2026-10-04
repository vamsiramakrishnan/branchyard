//! The behaviour the rest of the workspace relies on: failures are logged
//! with their context, poisoning is reported once with the lock's name, and a
//! thread's panic reaches the sink it was given.

#![allow(clippy::let_underscore_must_use, clippy::panic, clippy::unwrap_used)] // tests: a panic is the failure report
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use branchyard_support::testing::capture;
use branchyard_support::{
    best_effort, best_effort_once, cleanup_dir, set_failure_sink, spawn_named, CondvarExt, Failure,
    LockExt, RwLockExt,
};

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
fn a_panic_while_holding_a_lock_logs_one_warning_naming_it() {
    let lock = Arc::new(Mutex::new(7));
    poison(&lock);
    assert!(lock.is_poisoned());

    let (values, events) = capture(|| {
        let first = *lock.lock_recovering("fixture counter");
        let second = *lock.lock_recovering("fixture counter");
        let third = *lock.lock_recovering("fixture counter");
        [first, second, third]
    });

    assert_eq!(
        values,
        [7, 7, 7],
        "the data survives and later locks succeed"
    );
    let warnings: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN)
        .collect();
    assert_eq!(warnings.len(), 1, "logged once, not per lock: {events:?}");
    assert!(
        warnings[0].text.contains("fixture counter"),
        "names the lock: {}",
        warnings[0].text
    );
    assert!(!lock.is_poisoned(), "the flag is cleared after the warning");
}

#[test]
fn a_healthy_lock_logs_nothing() {
    let lock = Mutex::new(1);
    let (_, events) = capture(|| drop(lock.lock_recovering("quiet")));
    assert!(events.is_empty(), "{events:?}");
}

#[test]
fn rwlock_and_owned_access_recover_too() {
    let rw = Arc::new(RwLock::new(String::from("kept")));
    let held = rw.clone();
    let _ = std::thread::spawn(move || {
        let _w = held.write().unwrap();
        panic!("poisoning on purpose");
    })
    .join();
    let (text, events) = capture(|| rw.read_recovering("fixture rw").clone());
    assert_eq!(text, "kept");
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(events[0].text.contains("fixture rw"));
    drop(rw.write_recovering("fixture rw"));

    let mut owned = Mutex::new(5);
    *owned.get_mut_recovering("owned") += 1;
    assert_eq!(owned.into_inner_recovering("owned"), 6);
}

#[test]
fn condvar_waits_recover_from_poison() {
    let pair = Arc::new((Mutex::new(0u32), Condvar::new()));
    let held = pair.clone();
    let _ = std::thread::spawn(move || {
        let _guard = held.0.lock().unwrap();
        panic!("poisoning on purpose");
    })
    .join();
    let (state, wake) = &*pair;
    let guard = state.lock_recovering("pair");
    let (guard, timed_out) = wake.wait_timeout_recovering(guard, Duration::from_millis(5), "pair");
    assert_eq!(*guard, 0);
    assert!(timed_out.timed_out());
}

#[test]
fn best_effort_on_err_returns_none_and_logs_the_context() {
    let (value, events) = capture(|| {
        best_effort::<(), _>(
            "remove the staging directory",
            Err(std::io::Error::other("disk on fire")),
        )
    });
    assert_eq!(value, None);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].level, tracing::Level::WARN);
    assert!(
        events[0].text.contains("remove the staging directory"),
        "{}",
        events[0].text
    );
    assert!(
        events[0].text.contains("disk on fire"),
        "{}",
        events[0].text
    );
}

#[test]
fn best_effort_on_ok_returns_the_value_silently() {
    let (value, events) = capture(|| branchyard_support::best_effort!("fine", Ok::<_, String>(3)));
    assert_eq!(value, Some(3));
    assert!(events.is_empty());
}

#[test]
fn cleanup_dir_is_quiet_for_a_missing_directory_and_removes_a_present_one() {
    let scratch = branchyard_testkit::Scratch::new("support-cleanup");
    let dir = scratch.join("victim");
    std::fs::create_dir_all(dir.join("inner")).unwrap();
    let (_, events) = capture(|| {
        cleanup_dir(&dir);
        cleanup_dir(&dir);
    });
    assert!(!dir.exists());
    assert!(events.is_empty(), "{events:?}");
}

#[test]
fn a_panicking_thread_reaches_its_sink_and_the_log() {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = spawn_named(
        "by-fixture",
        move |report| tx.send(report).unwrap(),
        || panic!("worker blew up"),
    )
    .unwrap();
    handle.join().expect("the panic was contained");
    let report = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(report.thread, "by-fixture");
    assert!(report.message.contains("worker blew up"), "{report:?}");
}

#[test]
fn a_thread_that_does_not_panic_never_calls_its_sink() {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let handle = spawn_named("by-fine", move |_| tx.send(()).unwrap(), || {}).unwrap();
    handle.join().unwrap();
    assert!(rx.try_recv().is_err());
}

// One test owns the process-wide sink so parallel tests cannot see each
// other's failures through it.
#[test]
fn the_failure_sink_receives_every_survived_failure() {
    let seen = Arc::new(Mutex::new(Vec::<Failure>::new()));
    let into = seen.clone();
    set_failure_sink(Arc::new(move |failure: &Failure| {
        into.lock_recovering("seen").push(failure.clone())
    }));
    let _ = best_effort::<(), _>("unique-sink-context", Err("nope"));
    let seen = seen.lock_recovering("seen");
    assert!(
        seen.iter()
            .any(|f| f.context == "unique-sink-context" && f.error == "nope"),
        "{seen:?}"
    );
}

#[test]
fn a_persistent_failure_in_a_loop_logs_once_and_its_recovery_logs_again() {
    let (_, events) = capture(|| {
        for _ in 0..50 {
            best_effort_once("poll the fixture log", Err::<(), _>("disk gone"));
        }
        best_effort_once("poll the fixture log", Ok::<_, String>(()));
        best_effort_once("poll the fixture log", Ok::<_, String>(()));
        for _ in 0..50 {
            best_effort_once("poll the fixture log", Err::<(), _>("disk gone again"));
        }
    });
    let warnings: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN)
        .collect();
    assert_eq!(
        warnings.len(),
        2,
        "once per outage, not per tick: {events:?}"
    );
    assert!(warnings[0].text.contains("poll the fixture log"));
    let recoveries: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::INFO)
        .collect();
    assert_eq!(
        recoveries.len(),
        1,
        "the recovery is logged once: {events:?}"
    );
    assert!(recoveries[0].text.contains("recovered"));
}

#[test]
fn failures_of_different_steps_are_told_apart() {
    let (_, events) = capture(|| {
        best_effort_once("cancel the fixture branch a", Err::<(), _>("lost"));
        best_effort_once("cancel the fixture branch b", Err::<(), _>("lost"));
        best_effort_once("cancel the fixture branch a", Err::<(), _>("lost"));
    });
    assert_eq!(events.len(), 2, "{events:?}");
}
