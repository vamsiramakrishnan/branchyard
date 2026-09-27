//! One conformance suite for every [`Backend`]: SQLite always, PostgreSQL
//! with the `postgres` feature when `BY_TEST_POSTGRES_URL` names a
//! database the tests may create tables in. Each test gets its own store:
//! a fresh SQLite file, or a fresh repository scope in the database.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use crate::state::{now_ms, Acquired, Backend, Begun, Fence, Owner, ProcessRow, Record};
use crate::storage::StorageBackend;
use crate::{Activity, BranchStatus, Error, RecordedEvent, SteerState};

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

pub(crate) fn steers(s: Opened) {
    let store = &s.backend;
    store.reserve("b", &owner("a")).unwrap();
    assert!(matches!(
        store.request_steer("nope", "x", "t", None),
        Err(Error::UnknownBranch(_))
    ));
    assert_eq!(
        store.request_steer("b", "x", "t", None).unwrap(),
        None,
        "no turn"
    );
    let fence = granted(store.acquire(&record("b"), &owner("a"), TTL).unwrap());
    let first = store
        .request_steer("b", "alice", "one", None)
        .unwrap()
        .unwrap();
    let second = store
        .request_steer("b", "bob", "two", None)
        .unwrap()
        .unwrap();
    assert!(second > first);
    let pending = store.pending_steers(&fence).unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|r| (
                r.id,
                r.by.as_str(),
                r.text.as_str(),
                r.turn,
                r.state.clone()
            ))
            .collect::<Vec<_>>(),
        [
            (first, "alice", "one", fence.turn, SteerState::Pending),
            (second, "bob", "two", fence.turn, SteerState::Pending)
        ]
    );
    assert!(pending
        .iter()
        .all(|r| r.branch == "b" && r.requested_ms > 0));
    store
        .settle_steer(&fence, first, &SteerState::Delivered)
        .unwrap();
    store
        .settle_steer(&fence, first, &SteerState::Accepted)
        .unwrap();
    let refused = SteerState::Refused {
        reason: "no".into(),
    };
    store.settle_steer(&fence, second, &refused).unwrap();
    assert!(store.pending_steers(&fence).unwrap().is_empty());
    assert_eq!(
        store.steer("b", first).unwrap().map(|r| r.state),
        Some(SteerState::Accepted)
    );
    assert_eq!(
        store.steer("b", second).unwrap().map(|r| r.state),
        Some(refused)
    );
    assert_eq!(store.steer("b", second + 100).unwrap(), None);

    // Settling is fenced: an engine whose lease was taken over cannot.
    let third = store
        .request_steer("b", "carol", "three", None)
        .unwrap()
        .unwrap();
    let row = store.leases().unwrap().remove(0);
    let taken = store.take_over(&row, &owner("b"), TTL).unwrap().unwrap();
    assert!(matches!(
        store.settle_steer(&fence, third, &SteerState::Delivered),
        Err(Error::Fenced(_))
    ));
    // The takeover keeps the turn, so its input is still that turn's.
    assert_eq!(store.pending_steers(&taken).unwrap().len(), 1);

    // Bound to its turn: the next turn does not see it.
    store.finish(&taken, None, None).unwrap();
    let next = granted(store.acquire(&record("b"), &owner("a"), TTL).unwrap());
    assert!(store.pending_steers(&next).unwrap().is_empty());
    let fourth = store
        .request_steer("b", "dan", "four", None)
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .pending_steers(&next)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        [fourth]
    );
    store.finish(&next, None, None).unwrap();
    store.delete("b").unwrap();
    assert_eq!(store.steer("b", fourth).unwrap(), None);
}

pub(crate) fn messages(s: Opened) {
    let store = &s.backend;
    let sent = store
        .send_message(&crate::Message {
            id: 0,
            from: "kid".into(),
            to: "parent".into(),
            kind: crate::MessageKind::Question,
            text: "should I rename the module?".into(),
            in_reply_to: None,
            at_ms: 0,
            delivered: false,
        })
        .unwrap();
    assert!(sent.id > 0);
    assert!(sent.at_ms > 0);
    assert!(!sent.delivered);
    assert_eq!(store.message(sent.id).unwrap().as_ref(), Some(&sent));
    assert_eq!(store.message(sent.id + 999).unwrap(), None);

    // A second, unrelated message to someone else does not show up in
    // parent's inbox or as an answer to the first.
    store
        .send_message(&crate::Message {
            id: 0,
            from: "other".into(),
            to: "elsewhere".into(),
            kind: crate::MessageKind::Report,
            text: "tests pass".into(),
            in_reply_to: None,
            at_ms: 0,
            delivered: false,
        })
        .unwrap();

    let inbox = store.inbox("parent").unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].id, sent.id);
    assert!(!inbox[0].delivered);
    assert!(store.answer_to(sent.id).unwrap().is_none());

    store.mark_delivered(&[sent.id]).unwrap();
    assert!(store.inbox("parent").unwrap()[0].delivered);
    // Delivering twice, or an id nobody sent, is not an error.
    store.mark_delivered(&[sent.id, sent.id + 999]).unwrap();

    let answer = store
        .send_message(&crate::Message {
            id: 0,
            from: "parent".into(),
            to: "kid".into(),
            kind: crate::MessageKind::Answer,
            text: "yes, rename it".into(),
            in_reply_to: Some(sent.id),
            at_ms: 0,
            delivered: false,
        })
        .unwrap();
    assert_eq!(store.answer_to(sent.id).unwrap().as_ref(), Some(&answer));
    assert_eq!(store.inbox("kid").unwrap(), [answer]);

    let note = |from: &str, to: &str, kind: crate::MessageKind| {
        store
            .send_message(&crate::Message {
                id: 0,
                from: from.into(),
                to: to.into(),
                kind,
                text: "note".into(),
                in_reply_to: None,
                at_ms: 0,
                delivered: false,
            })
            .unwrap()
    };
    let delivered = |id: u64| store.message(id).unwrap().unwrap().delivered;

    // Steering a message: linked in the steer's own transaction, delivered
    // as that input settles written or accepted, pending again if refused.
    store.reserve("m", &owner("a")).unwrap();
    let fence = granted(store.acquire(&record("m"), &owner("a"), TTL).unwrap());
    let one = note("kid", "m", crate::MessageKind::Report);
    let first = store
        .request_steer("m", "kid", "one", Some(one.id))
        .unwrap()
        .unwrap();
    assert_eq!(store.message_steer(one.id).unwrap(), Some(first));
    let row = store.pending_steers(&fence).unwrap().remove(0);
    assert_eq!((row.message, row.message_delivered), (Some(one.id), false));
    // A delivered or unknown message is refused, and nothing is queued.
    for id in [sent.id, one.id + 999] {
        assert!(matches!(
            store.request_steer("m", "kid", "x", Some(id)),
            Err(Error::Denied(_))
        ));
    }
    assert_eq!(store.pending_steers(&fence).unwrap().len(), 1);
    assert_eq!(
        store
            .settle_steer(&fence, first, &SteerState::Delivered)
            .unwrap(),
        Some(one.id)
    );
    assert!(delivered(one.id));
    assert_eq!(
        store
            .settle_steer(&fence, first, &SteerState::Accepted)
            .unwrap(),
        None,
        "delivered once"
    );
    let refused = SteerState::Refused {
        reason: "dropped".into(),
    };
    assert_eq!(store.settle_steer(&fence, first, &refused).unwrap(), None);
    assert!(!delivered(one.id), "refused after all: pending again");
    assert_eq!(store.message_steer(one.id).unwrap(), None);

    // The turn's start delivers it first: the queued input sees that, and
    // refusing it does not undo the delivery.
    let second = store
        .request_steer("m", "kid", "one", Some(one.id))
        .unwrap()
        .unwrap();
    store.mark_delivered(&[one.id]).unwrap();
    let row = store.pending_steers(&fence).unwrap().remove(0);
    assert_eq!((row.id, row.message_delivered), (second, true));
    assert_eq!(store.settle_steer(&fence, second, &refused).unwrap(), None);
    assert!(delivered(one.id));
    store.finish(&fence, None, None).unwrap();

    // A waiter for an answer counts until its deadline or the answer.
    let question = note("asker", "parent", crate::MessageKind::Question);
    assert!(!store.awaiting_answer("asker", 1_000).unwrap());
    store.set_awaiting(question.id, Some(5_000)).unwrap();
    assert!(store.awaiting_answer("asker", 1_000).unwrap());
    assert!(!store.awaiting_answer("asker", 6_000).unwrap(), "past it");
    assert!(!store.awaiting_answer("parent", 1_000).unwrap());
    store.set_awaiting(question.id, None).unwrap();
    assert!(!store.awaiting_answer("asker", 1_000).unwrap());
    store.set_awaiting(question.id, Some(5_000)).unwrap();
    let mut reply = note("parent", "asker", crate::MessageKind::Answer);
    reply.in_reply_to = Some(question.id);
    assert!(store.awaiting_answer("asker", 1_000).unwrap());
    store.send_message(&reply).unwrap();
    assert!(!store.awaiting_answer("asker", 1_000).unwrap(), "answered");
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
/// (ancestor, descendant, sibling refused until shared) decided on
/// incarnations, a parent bound at creation and kept after its removal, a
/// reused name inheriting no share or lock, digest dedup and refcounting,
/// and a scratch lock reclaimed once its holder stops running.
pub(crate) fn storage(s: Opened) {
    use crate::storage::{Grants, Lineage, LockOutcome, NewArtifact, NewScratch};
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
    let lineage = || Lineage::from_identities(storage.identities().unwrap());
    let id = |name: &str| lineage().id(name).unwrap();
    let (root, a, aa, b) = (id("root"), id("a"), id("aa"), id("b"));
    let identities = storage.identities().unwrap();
    let parent_of = |name: &str| {
        identities
            .iter()
            .find(|i| i.name == name)
            .unwrap()
            .parent_incarnation
    };
    // Each parent was bound when its child's record was first written.
    assert_eq!(parent_of("root"), None);
    assert_eq!(parent_of("a"), Some(root));
    assert_eq!(parent_of("aa"), Some(a));
    assert!(root < a && a < aa && aa < b);

    // Publishing the same bytes twice from different branches is recorded
    // as two artifacts sharing one digest; each gets its own id.
    let new = |publisher: &str, incarnation: i64, ancestry: Vec<i64>| NewArtifact {
        digest: "d0".into(),
        size: 3,
        name: "n".into(),
        media_type: "text/plain".into(),
        publisher_branch: publisher.into(),
        turn: 1,
        labels: Default::default(),
        ancestry: Vec::new(),
        publisher_incarnation: incarnation,
        ancestry_incarnations: ancestry,
    };
    let by_aa = storage
        .create_artifact(&new("aa", aa, vec![root, a]))
        .unwrap();
    let by_b = storage.create_artifact(&new("b", b, vec![root])).unwrap();
    assert_ne!(by_aa.artifact.id, by_b.artifact.id);
    assert_eq!(by_aa.publisher_incarnation, Some(aa));
    assert_eq!(
        storage.artifact(&by_aa.artifact.id).unwrap().as_ref(),
        Some(&by_aa)
    );
    assert_eq!(storage.digest_refcount("d0").unwrap(), 2);

    let readable = |reader: i64, id: &str| {
        let row = storage.artifact(id).unwrap().unwrap();
        let shares = storage.artifact_shares(id).unwrap();
        Grants {
            owner: row.publisher_incarnation,
            ancestry: &row.ancestry_incarnations,
            shares: &shares,
        }
        .readable(&lineage(), reader)
    };
    // root and a (ancestors of aa) can read; b (a's sibling) cannot until
    // shared; aa reads b's never, and a descendant reads its ancestor's.
    assert!(readable(root, &by_aa.artifact.id));
    assert!(readable(a, &by_aa.artifact.id));
    assert!(!readable(b, &by_aa.artifact.id));
    assert!(storage.share_artifact(&by_aa.artifact.id, "b", b).unwrap());
    assert!(readable(b, &by_aa.artifact.id));
    assert!(!storage.share_artifact("unknown", "b", b).unwrap());

    // b is removed and an unrelated branch takes its name: a new
    // incarnation, which the share to the old b does not reach.
    branches.delete("b").unwrap();
    branches
        .write(&record_with_parent("b", None), None)
        .unwrap();
    let b2 = id("b");
    assert!(b2 > b);
    assert!(!readable(b2, &by_aa.artifact.id));
    assert!(!readable(b2, &by_b.artifact.id), "b2 is not b's publisher");
    // Sharing again binds the name's current holder, replacing the old.
    assert!(storage.share_artifact(&by_aa.artifact.id, "b", b2).unwrap());
    assert!(readable(b2, &by_aa.artifact.id));
    assert_eq!(
        storage.artifact_shares(&by_aa.artifact.id).unwrap().len(),
        1
    );

    // a is removed: aa's parent stays bound, so aa still reads what a
    // published, and a new holder of the name a is nobody's parent.
    let by_a = storage.create_artifact(&new("a", a, vec![root])).unwrap();
    branches.delete("a").unwrap();
    branches
        .write(&record_with_parent("a", None), None)
        .unwrap();
    let a2 = id("a");
    assert!(readable(aa, &by_a.artifact.id));
    assert!(!readable(a2, &by_a.artifact.id));
    assert!(!readable(a2, &by_aa.artifact.id));
    assert!(readable(root, &by_a.artifact.id));

    assert_eq!(storage.artifacts().unwrap().len(), 3);
    storage.delete_artifact(&by_aa.artifact.id).unwrap();
    assert_eq!(storage.digest_refcount("d0").unwrap(), 2);
    assert!(storage.artifact(&by_aa.artifact.id).unwrap().is_none());
    assert!(storage
        .artifact_shares(&by_aa.artifact.id)
        .unwrap()
        .is_empty());

    // Scratch: one writer at a time, reclaimed once the holder is no
    // longer running.
    let scratch = |name: &str, owner: &str, incarnation: i64| NewScratch {
        name: name.into(),
        owner: owner.into(),
        owner_incarnation: incarnation,
        ancestry: vec!["root".into()],
        ancestry_incarnations: vec![root],
    };
    assert!(storage.create_scratch(&scratch("cache", "aa", aa)).unwrap());
    assert!(!storage.create_scratch(&scratch("cache", "b", b2)).unwrap());
    let row = storage.scratch("cache").unwrap().unwrap();
    assert_eq!(row.owner_incarnation, Some(aa));
    assert_eq!(row.ancestry_incarnations, vec![root]);
    assert!(storage.share_scratch("cache", "b", b2).unwrap());
    assert_eq!(
        storage.scratch_shares("cache").unwrap()[0].incarnation,
        Some(b2)
    );
    match storage.scratch_lock("cache", "aa", aa).unwrap().unwrap() {
        LockOutcome::Granted(lock) => assert_eq!(lock.holder_branch, "aa"),
        LockOutcome::Held(_) => panic!("a free lock was reported held"),
    }
    // Re-entrant for its own holder.
    assert!(matches!(
        storage.scratch_lock("cache", "aa", aa).unwrap().unwrap(),
        LockOutcome::Granted(_)
    ));
    // The backend enforces only the lock, not the grant (the grant is
    // `crate::storage`'s job); it finds aa's turn not running, so it
    // reclaims the lock for b.
    match storage.scratch_lock("cache", "b", b2).unwrap().unwrap() {
        LockOutcome::Granted(lock) => assert_eq!(lock.holder_branch, "b"),
        LockOutcome::Held(_) => panic!("a lock whose holder is not running was still held"),
    }
    let mut running = record_with_parent("root", None);
    running.info.status = BranchStatus::Running;
    branches.write(&running, None).unwrap();
    assert!(storage
        .create_scratch(&scratch("busy", "root", root))
        .unwrap());
    storage.scratch_lock("busy", "root", root).unwrap();
    match storage.scratch_lock("busy", "b", b2).unwrap().unwrap() {
        LockOutcome::Held(lock) => assert_eq!(lock.holder_branch, "root"),
        LockOutcome::Granted(_) => panic!("a live holder's lock was reclaimed"),
    }
    assert!(!storage.scratch_unlock("busy", b2).unwrap());
    assert!(storage.scratch_unlock("busy", root).unwrap());
    assert!(storage.scratch_lock_state("busy").unwrap().is_none());
    // A running holder that is removed is not running any more, and a new
    // holder of its name does not hold its lock.
    storage.scratch_lock("busy", "root", root).unwrap();
    branches.delete("root").unwrap();
    branches.write(&running, None).unwrap();
    let root2 = id("root");
    assert!(!storage.scratch_unlock("busy", root2).unwrap());
    assert!(matches!(
        storage.scratch_lock("busy", "b", b2).unwrap().unwrap(),
        LockOutcome::Granted(_)
    ));
    assert_eq!(storage.scratch_list().unwrap().len(), 2);
    storage.delete_scratch("busy").unwrap();
    assert!(storage.scratch("busy").unwrap().is_none());
}

/// Upgrading a schema 1 store binds its name-only grants once, by
/// [`crate::storage::LegacyBinder`]'s rule. `downgrade` turns the freshly
/// opened store back into schema 1 (dropping the identity columns) and
/// inserts legacy rows through `legacy`'s SQL, with `{p}` the table prefix
/// and `{r}`/`{rv}` the repository column and value, if any; `reopen`
/// upgrades it.
pub(crate) fn upgrade(
    backend: &dyn Backend,
    exec: &mut dyn FnMut(&str),
    prefix: &str,
    repo: Option<&str>,
    reopen: &dyn Fn() -> Arc<dyn StorageBackend>,
) {
    use crate::storage::Lineage;
    let at = |name: &str, parent: Option<&str>, created_ms: u64| {
        let mut record = record_with_parent(name, parent);
        record.created_ms = created_ms;
        record
    };
    backend.write(&at("root", None, 0), None).unwrap();
    backend.write(&at("kid", Some("root"), 10), None).unwrap();
    // Took its name after the artifacts below were published.
    backend.write(&at("late", None, 5000), None).unwrap();
    let (r, rv) = match repo {
        Some(repo) => ("repo, ".to_owned(), format!("'{repo}', ")),
        None => (String::new(), String::new()),
    };
    let p = prefix;
    for (table, column) in [
        ("branches", "parent_incarnation"),
        ("artifacts", "publisher_incarnation"),
        ("artifacts", "ancestry_incarnations"),
        ("artifact_shares", "incarnation"),
        ("scratch_areas", "owner_incarnation"),
        ("scratch_areas", "ancestry_incarnations"),
        ("scratch_shares", "incarnation"),
        ("scratch_locks", "holder_incarnation"),
    ] {
        exec(&format!("ALTER TABLE {p}{table} DROP COLUMN {column}"));
    }
    exec(&format!(
        "UPDATE {p}meta SET value = '1' WHERE key = 'schema'"
    ));
    for (id, publisher) in [("art-kid", "kid"), ("art-late", "late")] {
        exec(&format!(
            "INSERT INTO {p}artifacts ({r}id, digest, size, name, media_type, publisher, \
             ancestry, turn, created_ms, labels) VALUES ({rv}'{id}', 'd', 1, 'n', 't', \
             '{publisher}', '[\"root\", \"gone\"]', 1, 1000, '{{}}')"
        ));
    }
    exec(&format!(
        "INSERT INTO {p}artifact_shares ({r}id, branch) VALUES ({rv}'art-late', 'kid')"
    ));
    exec(&format!(
        "INSERT INTO {p}artifact_shares ({r}id, branch) VALUES ({rv}'art-late', 'gone')"
    ));
    exec(&format!(
        "INSERT INTO {p}scratch_areas ({r}name, owner, ancestry, created_ms) \
         VALUES ({rv}'s', 'kid', '[\"root\"]', 1000)"
    ));
    exec(&format!(
        "INSERT INTO {p}scratch_shares ({r}name, branch) VALUES ({rv}'s', 'late')"
    ));
    exec(&format!(
        "INSERT INTO {p}scratch_locks ({r}name, holder, acquired_ms) VALUES ({rv}'s', 'late', 1000)"
    ));

    let storage = reopen();
    let identities = storage.identities().unwrap();
    let lineage = Lineage::from_identities(identities.clone());
    let (root, kid) = (lineage.id("root").unwrap(), lineage.id("kid").unwrap());
    let kid_identity = identities.iter().find(|i| i.name == "kid").unwrap();
    assert_eq!(kid_identity.parent_incarnation, Some(root));

    // Bound where a branch holding the name existed when it published.
    let art_kid = storage.artifact("art-kid").unwrap().unwrap();
    assert_eq!(art_kid.publisher_incarnation, Some(kid));
    assert_eq!(art_kid.ancestry_incarnations, vec![root]);
    assert_eq!(art_kid.ancestry, vec!["root".to_owned(), "gone".to_owned()]);
    // Not bound: late took the name after the artifact was published.
    let art_late = storage.artifact("art-late").unwrap().unwrap();
    assert_eq!(art_late.publisher_incarnation, None);
    let mut shares = storage.artifact_shares("art-late").unwrap();
    shares.sort_by(|x, y| x.branch.cmp(&y.branch));
    assert_eq!(
        shares
            .iter()
            .map(|s| (s.branch.as_str(), s.incarnation))
            .collect::<Vec<_>>(),
        vec![("gone", None), ("kid", Some(kid))]
    );
    let area = storage.scratch("s").unwrap().unwrap();
    assert_eq!(area.owner_incarnation, Some(kid));
    assert_eq!(area.ancestry_incarnations, vec![root]);
    assert_eq!(
        storage.scratch_shares("s").unwrap()[0].incarnation,
        Some(lineage.id("late").unwrap())
    );
    // late's lock was taken before late existed: unbound, so reclaimable
    // by anyone and releasable by no one.
    assert!(!storage
        .scratch_unlock("s", lineage.id("late").unwrap())
        .unwrap());
    // Upgrading is done once.
    let again = reopen();
    assert_eq!(
        again
            .artifact("art-kid")
            .unwrap()
            .unwrap()
            .publisher_incarnation,
        Some(kid)
    );
}

/// Generate one `#[test]` per conformance check for a backend.
macro_rules! suite {
    ($open:expr) => {
        suite!($open; fencing, expiry, steps, cancels, steers, records, reservations, events,
            concurrent_appends, races, storage, messages);
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

    #[test]
    fn upgrade() {
        use crate::sqlite::Sqlite;
        use std::sync::Arc;
        let dir = std::env::temp_dir().join(format!(
            "branchyard-conformance-{}",
            super::unique("upgrade")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let backend = Sqlite::open(&dir).unwrap();
        let conn = rusqlite::Connection::open(dir.join("state.db")).unwrap();
        crate::conformance::upgrade(
            &backend,
            &mut |sql| conn.execute_batch(sql).unwrap(),
            "",
            None,
            &|| Arc::new(Sqlite::open(&dir).unwrap()),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(feature = "postgres")]
mod postgres {
    suite!(crate::conformance::postgres);

    /// In a schema of its own: it drops columns, which would break every
    /// other test sharing the tables.
    #[test]
    fn upgrade() {
        use crate::pg::Postgres;
        use std::sync::Arc;
        let Some(base) = crate::conformance::postgres_url() else {
            eprintln!("skipped: set BY_TEST_POSTGRES_URL to run the PostgreSQL conformance tests");
            return;
        };
        let schema = format!("by_upgrade_{}", super::unique("x").replace('-', "_"));
        let mut admin = crate::pg::connect(&base).unwrap();
        admin
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {schema} CASCADE; CREATE SCHEMA {schema}"
            ))
            .unwrap();
        let separator = if base.contains('?') { '&' } else { '?' };
        let url = format!("{base}{separator}options=-csearch_path%3D{schema}");
        let backend = Postgres::open(&url, "repo").unwrap();
        let mut raw = crate::pg::connect(&url).unwrap();
        crate::conformance::upgrade(
            &backend,
            &mut |sql| raw.batch_execute(sql).unwrap(),
            "by_",
            Some("repo"),
            &|| Arc::new(Postgres::open(&url, "repo").unwrap()),
        );
        drop(raw);
        drop(backend);
        admin
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .unwrap();
    }
}
