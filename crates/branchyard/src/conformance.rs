//! One conformance suite for every [`Backend`]: SQLite always, PostgreSQL
//! with the `postgres` feature when `BY_TEST_POSTGRES_URL` names a
//! database the tests may create tables in. Each test gets its own store:
//! a fresh SQLite file, or a fresh repository scope in the database.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use crate::state::{now_ms, Acquired, Backend, Begun, Fence, Owner, ProcessRow, Record};
use crate::storage::StorageBackend;
use crate::{Activity, BranchStatus, Error, RecordedEvent};

const TTL: Duration = Duration::from_secs(30);

/// A store for one test, and whatever must outlive it.
pub(crate) struct Opened {
    pub backend: Arc<dyn Backend>,
    /// The same backend, as [`StorageBackend`]: see [`crate::storage`].
    pub storage: Arc<dyn StorageBackend>,
    /// Opens another handle on the same store, as a second engine would.
    pub again: Box<dyn Fn() -> Arc<dyn Backend>>,
    _cleanup: Box<dyn std::any::Any>,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique(name: &str) -> String {
    format!(
        "{}-{}-{name}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

pub(crate) fn sqlite(name: &str) -> Opened {
    struct Temp(std::path::PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let dir = std::env::temp_dir().join(format!("branchyard-conformance-{}", unique(name)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let shared = Arc::new(crate::sqlite::Sqlite::open(&dir).unwrap());
    let open = {
        let dir = dir.clone();
        move || Arc::new(crate::sqlite::Sqlite::open(&dir).unwrap()) as Arc<dyn Backend>
    };
    Opened {
        backend: shared.clone(),
        storage: shared,
        again: Box::new(open),
        _cleanup: Box::new(Temp(dir)),
    }
}

/// The database the PostgreSQL tests use, if one is configured.
#[cfg(feature = "postgres")]
pub(crate) fn postgres_url() -> Option<String> {
    std::env::var("BY_TEST_POSTGRES_URL")
        .ok()
        .filter(|u| !u.is_empty())
}

#[cfg(feature = "postgres")]
pub(crate) fn postgres(name: &str) -> Option<Opened> {
    let url = postgres_url()?;
    let scope = format!("conformance-{}", unique(name));
    let shared = Arc::new(crate::pg::Postgres::open(&url, &scope).unwrap());
    let open = {
        let url = url.clone();
        let scope = scope.clone();
        move || Arc::new(crate::pg::Postgres::open(&url, &scope).unwrap()) as Arc<dyn Backend>
    };
    Some(Opened {
        backend: shared.clone(),
        storage: shared,
        again: Box::new(open),
        _cleanup: Box::new(()),
    })
}

fn record(name: &str) -> Record {
    serde_json::from_value(serde_json::json!({
        "info": {
            "name": name, "git_branch": format!("by/{name}"), "worktree": "/w",
            "prompt": "p", "harness": "h", "profile": "p", "session": null,
            "parent": null, "base": "b", "candidate": null,
            "status": {"state": "running"}, "turns": 0, "cost_usd": null,
            "created_at": 0
        },
        "created_ms": 0, "check": null, "command": null, "home": null,
        "cost_baseline": null
    }))
    .unwrap()
}

fn record_with_parent(name: &str, parent: Option<&str>) -> Record {
    let mut record = record(name);
    record.info.parent = parent.map(str::to_owned);
    record.info.status = BranchStatus::Ready;
    record
}

fn owner(id: &str) -> Owner {
    Owner {
        id: id.into(),
        host: crate::proc::host().into(),
        pid: std::process::id(),
        start: crate::proc::own_start().into(),
    }
}

fn event(n: u64) -> RecordedEvent {
    RecordedEvent {
        at_ms: n,
        activity: Activity::Warning(format!("event {n}")),
    }
}

fn granted(acquired: Acquired) -> Fence {
    match acquired {
        Acquired::Granted(fence) => fence,
        Acquired::Held(row) => panic!("held by {row:?}"),
    }
}

pub(crate) fn fencing(s: Opened) {
    let store = &s.backend;
    assert!(store.reserve("b", &owner("a")).unwrap());
    assert!(!store.reserve("b", &owner("a")).unwrap());
    let first = granted(store.acquire(&record("b"), &owner("a"), TTL).unwrap());
    assert_eq!((first.generation, first.turn), (1, 1));
    let Acquired::Held(row) = store.acquire(&record("b"), &owner("b"), TTL).unwrap() else {
        panic!("a held lease was granted twice");
    };
    assert_eq!(row.owner.as_deref(), Some("a"));
    assert_eq!(row.stale(now_ms()), None, "a live owner in this process");
    store.append("b", &event(1), Some(&first)).unwrap();

    let second = store.take_over(&row, &owner("b"), TTL).unwrap().unwrap();
    assert_eq!((second.generation, second.turn), (2, 1));
    assert_eq!(store.take_over(&row, &owner("c"), TTL).unwrap(), None);
    for refused in [
        store.write(&record("b"), Some(&first)),
        store.append("b", &event(2), Some(&first)).map(|_| ()),
        store.renew(&first, TTL),
        store.set_deadline(&first, Some(1)),
        store
            .begin_step(&first, 1, "x", &serde_json::json!({}))
            .map(|_| ()),
        store.record_process(
            &first,
            &ProcessRow {
                pid: 1,
                pgid: 1,
                start: "s".into(),
                host: "h".into(),
            },
        ),
        store.finish(&first, Some(&record("b")), None),
    ] {
        assert!(
            matches!(&refused, Err(Error::Fenced(why)) if why.contains("superseded")),
            "{refused:?}"
        );
    }
    assert_eq!(store.append("b", &event(3), Some(&second)).unwrap(), 2);
    store
        .finish(&second, Some(&record("b")), Some(&event(4)))
        .unwrap();
    assert!(matches!(
        store.renew(&second, TTL),
        Err(Error::Fenced(why)) if why.contains("released")
    ));
    assert!(store.leases().unwrap().is_empty());
    assert_eq!(store.event_count("b").unwrap(), 3);
}

pub(crate) fn expiry(s: Opened) {
    let store = &s.backend;
    store.reserve("b", &owner("a")).unwrap();
    let fence = granted(
        store
            .acquire(&record("b"), &owner("a"), Duration::ZERO)
            .unwrap(),
    );
    let row = store.leases().unwrap().remove(0);
    assert!(row.stale(now_ms()).unwrap().contains("expired"));
    store.set_deadline(&fence, Some(99)).unwrap();
    assert_eq!(store.leases().unwrap()[0].deadline_ms, Some(99));
    store.finish(&fence, None, None).unwrap();
    let next = granted(store.acquire(&record("b"), &owner("b"), TTL).unwrap());
    assert_eq!((next.generation, next.turn), (2, 2));
    assert_eq!(store.leases().unwrap()[0].deadline_ms, None);
}

pub(crate) fn steps(s: Opened) {
    let store = &s.backend;
    store.reserve("b", &owner("a")).unwrap();
    let fence = granted(store.acquire(&record("b"), &owner("a"), TTL).unwrap());
    let intent = serde_json::json!({ "prompt": "p" });
    assert_eq!(
        store.begin_step(&fence, 1, "submit", &intent).unwrap(),
        Begun::Fresh
    );
    assert_eq!(
        store.begin_step(&fence, 1, "submit", &intent).unwrap(),
        Begun::Pending(intent.clone())
    );
    let outcome = serde_json::json!({ "turn": 1 });
    store.finish_step(&fence, 1, "submit", &outcome).unwrap();
    assert_eq!(
        store.begin_step(&fence, 1, "submit", &intent).unwrap(),
        Begun::Done(outcome.clone())
    );
    store.abandon_step(&fence, 1, "submit").unwrap();
    store.begin_step(&fence, 1, "merge", &intent).unwrap();
    store.abandon_step(&fence, 1, "merge").unwrap();
    let steps = store.steps("b", 1).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].outcome, Some(outcome));
    assert!(store.finish_step(&fence, 2, "submit", &intent).is_err());
    assert!(store.steps("nope", 1).unwrap().is_empty());

    for pid in [7, 8, 7] {
        store
            .record_process(
                &fence,
                &ProcessRow {
                    pid,
                    pgid: 7,
                    start: format!("s{pid}"),
                    host: "h".into(),
                },
            )
            .unwrap();
    }
    let mut pids: Vec<u32> = store
        .processes("b", fence.turn)
        .unwrap()
        .iter()
        .map(|p| p.pid)
        .collect();
    pids.sort();
    assert_eq!(pids, [7, 8], "a repeated pid replaces its row");
    assert!(store.processes("b", 99).unwrap().is_empty());
}

pub(crate) fn cancels(s: Opened) {
    let store = &s.backend;
    store.reserve("b", &owner("a")).unwrap();
    assert!(matches!(
        store.request_cancel("nope", "x", false),
        Err(Error::UnknownBranch(_))
    ));
    assert!(!store.request_cancel("b", "x", false).unwrap(), "no turn");
    let fence = granted(store.acquire(&record("b"), &owner("a"), TTL).unwrap());
    assert!(store.request_cancel("b", "first", true).unwrap());
    assert!(store.request_cancel("b", "second", false).unwrap());
    assert_eq!(
        store.cancel_requested(&fence).unwrap().as_deref(),
        Some("first")
    );
    store.finish(&fence, None, None).unwrap();
    let next = granted(store.acquire(&record("b"), &owner("a"), TTL).unwrap());
    assert_eq!(store.cancel_requested(&next).unwrap(), None);
}

pub(crate) fn records(s: Opened) {
    let store = &s.backend;
    assert!(!store.taken("a").unwrap());
    assert!(store.reserve("a", &owner("a")).unwrap());
    assert!(store.taken("a").unwrap());
    assert_eq!(store.read("a").unwrap().map(|r| r.info.name), None);
    store.release("a").unwrap();
    assert!(!store.taken("a").unwrap());

    let mut older = record("z");
    older.created_ms = 1;
    let mut newer = record("a");
    newer.created_ms = 2;
    store.write(&newer, None).unwrap();
    store.write(&older, None).unwrap();
    // A written record is not released as a reservation.
    store.release("a").unwrap();
    let names: Vec<String> = store
        .list()
        .unwrap()
        .into_iter()
        .map(|r| r.info.name)
        .collect();
    assert_eq!(names, ["z", "a"]);

    store.add_child("a", "kid").unwrap();
    store.add_child("a", "kid").unwrap();
    store.add_child("a", "other").unwrap();
    assert!(matches!(
        store.add_child("nope", "kid"),
        Err(Error::UnknownBranch(_))
    ));
    // Every other write keeps the stored children.
    let mut rewritten = newer.clone();
    rewritten.info.turns = 3;
    store.write(&rewritten, None).unwrap();
    let read = store.read("a").unwrap().unwrap();
    assert_eq!(read.info.children, ["kid", "other"]);
    assert_eq!(read.info.turns, 3);

    // A deleted branch's events stay in the feed; a new branch of that
    // name numbers its own from 1.
    store.append("a", &event(1), None).unwrap();
    store.append("z", &event(2), None).unwrap();
    store.delete("a").unwrap();
    store.delete("a").unwrap();
    assert!(!store.taken("a").unwrap());
    assert!(matches!(
        store.events_since("a", 0, 10),
        Err(Error::UnknownBranch(_))
    ));
    assert!(matches!(
        store.event_count("a"),
        Err(Error::UnknownBranch(_))
    ));
    store.write(&record("a"), None).unwrap();
    assert_eq!(store.append("a", &event(3), None).unwrap(), 1);
    let feed: Vec<(String, u64)> = store
        .feed_since(0, 10)
        .unwrap()
        .into_iter()
        .map(|f| (f.branch, f.event.at_ms))
        .collect();
    assert_eq!(feed, [("a".into(), 1), ("z".into(), 2), ("a".into(), 3)]);
}

pub(crate) fn reservations(s: Opened) {
    let store = &s.backend;
    assert!(store.reserve("r", &owner("a")).unwrap());
    let rows = store.reservations().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].name.as_str(), rows[0].owner.as_str()), ("r", "a"));
    // Only the reserving engine may create it.
    assert!(matches!(
        store.acquire(&record("r"), &owner("b"), TTL),
        Err(Error::BranchExists(_))
    ));
    // A stale read does not free a name reserved again since.
    let stale = rows[0].clone();
    assert!(store.reclaim(&stale).unwrap());
    assert!(!store.taken("r").unwrap());
    assert!(store.reserve("r", &owner("c")).unwrap());
    assert!(!store.reclaim(&stale).unwrap(), "reserved again since");
    assert!(store.taken("r").unwrap());
    // Creating it clears the reservation.
    granted(store.acquire(&record("r"), &owner("c"), TTL).unwrap());
    assert!(store.reservations().unwrap().is_empty());
    let mut current = stale;
    current.owner = "c".into();
    assert!(
        !store.reclaim(&current).unwrap(),
        "a created branch is kept"
    );
    assert!(store.taken("r").unwrap());
}

pub(crate) fn events(s: Opened) {
    let store = &s.backend;
    assert_eq!(store.head().unwrap(), 0);
    assert!(store.feed_since(0, 10).unwrap().is_empty());
    store.write(&record("a"), None).unwrap();
    store.write(&record("b"), None).unwrap();
    assert!(matches!(
        store.append("nope", &event(0), None),
        Err(Error::UnknownBranch(_))
    ));
    for n in 1..=6 {
        let branch = if n % 2 == 0 { "b" } else { "a" };
        assert_eq!(
            store.append(branch, &event(n), None).unwrap(),
            n.div_ceil(2)
        );
    }
    assert_eq!(store.event_count("a").unwrap(), 3);
    let page = store.events_since("a", 1, 10).unwrap();
    assert_eq!(
        page.iter()
            .map(|(seq, e)| (*seq, e.at_ms))
            .collect::<Vec<_>>(),
        [(2, 3), (3, 5)]
    );
    assert_eq!(store.events_since("a", 0, 1).unwrap().len(), 1);
    let feed = store.feed_since(0, 4).unwrap();
    assert_eq!(feed.len(), 4);
    assert!(feed.windows(2).all(|w| w[0].id < w[1].id));
    let rest = store.feed_since(feed[3].id, 10).unwrap();
    assert_eq!(
        rest.iter().map(|f| f.event.at_ms).collect::<Vec<_>>(),
        [5, 6]
    );
    assert_eq!(store.head().unwrap(), rest[1].id);
    assert_eq!(feed[0].event, event(1), "an activity round-trips unchanged");

    // A final event commits with the record and the lease's release.
    store.reserve("c", &owner("o")).unwrap();
    let fence = granted(store.acquire(&record("c"), &owner("o"), TTL).unwrap());
    let mut done = record("c");
    done.info.status = BranchStatus::Ready;
    let last = RecordedEvent {
        at_ms: 9,
        activity: Activity::Status(BranchStatus::Ready),
    };
    store.finish(&fence, Some(&done), Some(&last)).unwrap();
    assert_eq!(
        store.read("c").unwrap().unwrap().info.status,
        BranchStatus::Ready
    );
    assert_eq!(store.events_since("c", 0, 10).unwrap()[0].1, last);
    assert!(store.leases().unwrap().is_empty());
}

/// Appends from several threads, each through its own handle as separate
/// engines would, get distinct feed positions in commit order and
/// contiguous numbers per branch.
pub(crate) fn concurrent_appends(s: Opened) {
    let branches = 4;
    let each = 25;
    for b in 0..branches {
        s.backend.write(&record(&format!("b{b}")), None).unwrap();
    }
    let start = Arc::new(Barrier::new(branches));
    let threads: Vec<_> = (0..branches)
        .map(|b| {
            let store = (s.again)();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                (1..=each)
                    .map(|n| store.append(&format!("b{b}"), &event(n), None).unwrap())
                    .collect::<Vec<u64>>()
            })
        })
        .collect();
    for thread in threads {
        let seqs = thread.join().unwrap();
        assert_eq!(seqs, (1..=each).collect::<Vec<_>>());
    }
    let feed = s.backend.feed_since(0, 1000).unwrap();
    assert_eq!(feed.len(), branches * each as usize);
    assert!(feed.windows(2).all(|w| w[0].id < w[1].id));
    assert_eq!(s.backend.head().unwrap(), feed.last().unwrap().id);
    for b in 0..branches {
        let at: Vec<u64> = feed
            .iter()
            .filter(|f| f.branch == format!("b{b}"))
            .map(|f| f.event.at_ms)
            .collect();
        assert_eq!(at, (1..=each).collect::<Vec<_>>(), "per-branch order");
    }
}

/// Engines racing for one name, then one lease on the created branch, then
/// one takeover: exactly one wins each.
pub(crate) fn races(s: Opened) {
    let contenders = 4;
    for round in 0..5 {
        let name = format!("r{round}");
        let start = Arc::new(Barrier::new(contenders));
        let threads: Vec<_> = (0..contenders)
            .map(|i| {
                let store = (s.again)();
                let (start, name) = (start.clone(), name.clone());
                std::thread::spawn(move || {
                    start.wait();
                    let reserved = store.reserve(&name, &owner(&format!("o{i}"))).unwrap();
                    // Only the reserving engine may create the branch; it
                    // does, and lets go of the lease, before the lease race.
                    if reserved {
                        let fence = granted(
                            store
                                .acquire(&record(&name), &owner(&format!("o{i}")), TTL)
                                .unwrap(),
                        );
                        store.finish(&fence, None, None).unwrap();
                    }
                    start.wait();
                    let acquired = matches!(
                        store
                            .acquire(&record(&name), &owner(&format!("l{i}")), TTL)
                            .unwrap(),
                        Acquired::Granted(_)
                    );
                    start.wait();
                    let row = store
                        .leases()
                        .unwrap()
                        .into_iter()
                        .find(|l| l.branch == name)
                        .unwrap();
                    start.wait();
                    let took = store
                        .take_over(&row, &owner(&format!("t{i}")), TTL)
                        .unwrap()
                        .is_some();
                    (reserved, acquired, took)
                })
            })
            .collect();
        let results: Vec<(bool, bool, bool)> =
            threads.into_iter().map(|t| t.join().unwrap()).collect();
        let count = |f: fn(&(bool, bool, bool)) -> bool| results.iter().filter(|r| f(r)).count();
        assert_eq!(count(|r| r.0), 1, "one reservation: {results:?}");
        assert_eq!(count(|r| r.1), 1, "one lease: {results:?}");
        assert_eq!(count(|r| r.2), 1, "one takeover: {results:?}");
    }
}

/// Artifacts and scratch areas: [`crate::storage::StorageBackend`]. Grants
/// (ancestor, descendant, sibling refused until shared), digest dedup and
/// refcounting, and a scratch lock reclaimed once its holder stops running.
pub(crate) fn storage(s: Opened) {
    use crate::storage::{ArtifactRow, LockOutcome, NewArtifact};
    let branches = &s.backend;
    let storage = &s.storage;
    // root -> a -> aa; root -> b (a sibling of a).
    for (name, parent) in [
        ("root", None),
        ("a", Some("root")),
        ("aa", Some("a")),
        ("b", Some("root")),
    ] {
        branches
            .write(&record_with_parent(name, parent), None)
            .unwrap();
    }
    let ancestry_of = |name: &str| -> Vec<String> {
        let mut chain = Vec::new();
        let mut cur = name.to_owned();
        while let Some(record) = branches.read(&cur).unwrap() {
            match record.info.parent {
                Some(parent) => {
                    chain.push(parent.clone());
                    cur = parent;
                }
                None => break,
            }
        }
        chain.reverse();
        chain
    };

    // Publishing the same bytes twice from different branches is recorded
    // as two artifacts sharing one digest; each gets its own id.
    let new = |publisher: &str| NewArtifact {
        digest: "d0".into(),
        size: 3,
        name: "n".into(),
        media_type: "text/plain".into(),
        publisher_branch: publisher.into(),
        turn: 1,
        labels: Default::default(),
        ancestry: ancestry_of(publisher),
    };
    let published_by_aa = storage.create_artifact(&new("aa")).unwrap();
    let published_by_b = storage.create_artifact(&new("b")).unwrap();
    assert_ne!(published_by_aa.artifact.id, published_by_b.artifact.id);
    assert_eq!(storage.digest_refcount("d0").unwrap(), 2);

    let readable = |reader: &str, row: &ArtifactRow, shares: &[String]| {
        reader == row.artifact.publisher_branch
            || row.ancestry.iter().any(|a| a == reader)
            || shares.iter().any(|s| s == reader)
    };
    // root and a (ancestors of aa) can read; b (a's sibling) cannot until
    // shared.
    assert!(readable("root", &published_by_aa, &[]));
    assert!(readable("a", &published_by_aa, &[]));
    assert!(!readable("b", &published_by_aa, &[]));
    assert!(storage
        .share_artifact(&published_by_aa.artifact.id, "b")
        .unwrap());
    let shares = storage
        .artifact_shares(&published_by_aa.artifact.id)
        .unwrap();
    assert!(readable("b", &published_by_aa, &shares));
    assert!(!storage.share_artifact("unknown", "b").unwrap());

    assert_eq!(storage.artifacts().unwrap().len(), 2);
    storage
        .delete_artifact(&published_by_aa.artifact.id)
        .unwrap();
    assert_eq!(storage.digest_refcount("d0").unwrap(), 1);
    assert!(storage
        .artifact(&published_by_aa.artifact.id)
        .unwrap()
        .is_none());

    // Scratch: one writer at a time, reclaimed once the holder is no
    // longer running.
    assert!(storage
        .create_scratch("cache", "aa", &ancestry_of("aa"))
        .unwrap());
    assert!(!storage.create_scratch("cache", "b", &[]).unwrap());
    branches
        .write(&record_with_parent("aa", Some("a")), None)
        .unwrap(); // status Ready: not running
    match storage.scratch_lock("cache", "aa").unwrap().unwrap() {
        LockOutcome::Granted(lock) => assert_eq!(lock.holder_branch, "aa"),
        LockOutcome::Held(_) => panic!("a free lock was reported held"),
    }
    // Re-entrant for its own holder.
    assert!(matches!(
        storage.scratch_lock("cache", "aa").unwrap().unwrap(),
        LockOutcome::Granted(_)
    ));
    // b is a's sibling and not authorized here, but the backend enforces
    // only the lock, not the grant (the grant is `crate::storage`'s job);
    // it still finds aa's turn not running, so it reclaims the lock.
    match storage.scratch_lock("cache", "b").unwrap().unwrap() {
        LockOutcome::Granted(lock) => assert_eq!(lock.holder_branch, "b"),
        LockOutcome::Held(_) => panic!("a lock whose holder is not running was still held"),
    }
    let mut running = record_with_parent("root", None);
    running.info.status = BranchStatus::Running;
    branches.write(&running, None).unwrap();
    assert!(storage.create_scratch("busy", "root", &[]).unwrap());
    storage.scratch_lock("busy", "root").unwrap();
    match storage.scratch_lock("busy", "b").unwrap().unwrap() {
        LockOutcome::Held(lock) => assert_eq!(lock.holder_branch, "root"),
        LockOutcome::Granted(_) => panic!("a live holder's lock was reclaimed"),
    }
    assert!(!storage.scratch_unlock("busy", "b").unwrap());
    assert!(storage.scratch_unlock("busy", "root").unwrap());
    assert!(storage.scratch_lock_state("busy").unwrap().is_none());
    assert_eq!(storage.scratch_list().unwrap().len(), 2);
    storage.delete_scratch("busy").unwrap();
    assert!(storage.scratch("busy").unwrap().is_none());
}

/// Generate one `#[test]` per conformance check for a backend.
macro_rules! suite {
    ($open:expr) => {
        suite!($open; fencing, expiry, steps, cancels, records, reservations, events,
            concurrent_appends, races, storage);
    };
    ($open:expr; $($check:ident),*) => {
        $(
            #[test]
            fn $check() {
                match $open(stringify!($check)) {
                    Some(opened) => crate::conformance::$check(opened),
                    None => eprintln!(
                        "skipped: set BY_TEST_POSTGRES_URL to run the PostgreSQL conformance tests"
                    ),
                }
            }
        )*
    };
}

mod sqlite {
    suite!(|name| Some(crate::conformance::sqlite(name)));
}

#[cfg(feature = "postgres")]
mod postgres {
    suite!(crate::conformance::postgres);
}
