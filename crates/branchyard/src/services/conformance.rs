//! What every [`ServiceStore`] must do, as functions a backend's tests
//! call: [`check`] on one store, [`check_concurrent`] on several handles
//! to one registry. Times are given explicitly, so nothing waits.

use std::sync::{Arc, Barrier};

use super::{
    candidates, expire, reap, resolve, Capability, Endpoint, Health, Outcome, ProcessGroup, Query,
    Reclaim, Service, ServiceOwner, ServiceState, ServiceStore, KEEP_ENDED,
};

/// An owner on a host that is not this one: only its lease says whether
/// it lives.
pub fn owner(id: &str) -> ServiceOwner {
    ServiceOwner {
        id: id.to_owned(),
        host: "elsewhere/boot".into(),
        pid: 4242,
        start: "1".into(),
        branch: None,
        operation: None,
        principal: None,
    }
}

fn gateway(id: &str, owner_id: &str, connectors: &[&str], until: u64) -> Service {
    let mut service = Service::new("connector_gateway", owner(owner_id))
        .with_id(id)
        .with(
            "connectors",
            connectors
                .iter()
                .map(|c| (*c).to_owned())
                .collect::<Vec<_>>(),
        )
        .with("protocol", "mcp")
        .with_endpoint(Endpoint::url(format!("http://127.0.0.1:1/{id}")));
    service.lease_until_ms = until;
    service
}

/// Register, renew, resolve, expire, reap, deregister, refuse a second
/// owner, follow changes and prune, on `store`, which must be empty.
pub fn check(store: &dyn ServiceStore, label: &str) {
    let t0 = 1_000_000_u64;
    // Register: live, stamped, with the next change number.
    let a = store
        .register(
            &gateway("gw-a", "o-a", &["github", "slack"], t0 + 30_000),
            t0,
        )
        .unwrap();
    assert_eq!(a.state, ServiceState::Live, "{label}");
    assert_eq!((a.registered_ms, a.renewed_ms), (t0, t0), "{label}");
    assert!(a.seq > 0, "{label}");
    let b = store
        .register(
            &gateway("gw-b", "o-b", &["github"], t0 + 30_000).with_weight(5),
            t0,
        )
        .unwrap();
    assert!(b.seq > a.seq, "{label}: changes are numbered in order");

    // Resolve by capability: both serve github, b is heavier; only a
    // serves slack; nothing serves linear.
    let github = Query::kind("connector_gateway").require("connectors", "github");
    assert_eq!(
        resolve(store, &github, t0 + 1).unwrap().unwrap().id,
        "gw-b",
        "{label}"
    );
    let slack = Query::kind("connector_gateway").require("connectors", "slack");
    assert_eq!(
        resolve(store, &slack, t0 + 1).unwrap().unwrap().id,
        "gw-a",
        "{label}"
    );
    assert!(resolve(
        store,
        &Query::kind("connector_gateway").require("connectors", "linear"),
        t0 + 1
    )
    .unwrap()
    .is_none());
    assert!(resolve(store, &Query::kind("model_gateway"), t0 + 1)
        .unwrap()
        .is_none());

    // Health before weight: a degraded b loses to a healthy a.
    let renewed = store
        .renew("gw-b", "o-b", t0 + 40_000, Some(Health::Degraded), t0 + 2)
        .unwrap()
        .unwrap();
    assert_eq!(renewed.health, Health::Degraded, "{label}");
    assert_eq!(renewed.lease_until_ms, t0 + 40_000, "{label}");
    assert_eq!(
        candidates(store, &github, t0 + 3)
            .unwrap()
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        ["gw-a", "gw-b"],
        "{label}"
    );
    // Unhealthy is never resolved.
    store
        .renew("gw-b", "o-b", t0 + 40_000, Some(Health::Unhealthy), t0 + 4)
        .unwrap()
        .unwrap();
    assert_eq!(
        candidates(store, &github, t0 + 5).unwrap().len(),
        1,
        "{label}"
    );
    store
        .renew("gw-b", "o-b", t0 + 40_000, Some(Health::Healthy), t0 + 6)
        .unwrap()
        .unwrap();

    // Only the owner renews or deregisters; another owner cannot take a
    // live ID.
    assert!(store
        .renew("gw-a", "o-b", t0 + 90_000, None, t0 + 7)
        .unwrap()
        .is_none());
    assert!(!store.deregister("gw-a", "o-b", t0 + 7).unwrap(), "{label}");
    let taken = store.register(&gateway("gw-a", "o-x", &[], t0 + 90_000), t0 + 7);
    assert_eq!(
        taken.unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists,
        "{label}"
    );
    // Its owner registering again keeps when it first registered.
    let again = store
        .register(
            &gateway("gw-a", "o-a", &["github", "slack"], t0 + 30_000),
            t0 + 8,
        )
        .unwrap();
    assert_eq!(again.registered_ms, t0, "{label}");

    // A record with something to reclaim, whose lease runs out first.
    let mut leaked = Service::new("egress_proxy", owner("o-c"))
        .with_id("px-c")
        .with_reclaim(Reclaim::Process {
            host: "elsewhere/boot".into(),
            pid: 9,
            start: "1".into(),
            group: Some(ProcessGroup {
                pgid: 9,
                leader_start: "1".into(),
            }),
        });
    leaked.lease_until_ms = t0 + 10_000;
    store.register(&leaked, t0).unwrap();

    // Before any lease runs out nothing expires.
    assert!(expire(store, t0 + 9_999).unwrap().is_empty(), "{label}");
    // Past px-c's lease: it alone expires, once.
    let (head, _) = store.since(0).unwrap();
    let expired = expire(store, t0 + 10_000).unwrap();
    assert_eq!(
        expired.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["px-c"],
        "{label}"
    );
    assert_eq!(expired[0].state, ServiceState::Expired, "{label}");
    assert!(expire(store, t0 + 10_000).unwrap().is_empty(), "{label}");
    // Watching sees exactly that change.
    let (next, changed) = store.since(head).unwrap();
    assert_eq!(
        changed.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["px-c"],
        "{label}"
    );
    assert!(next > head, "{label}");
    // An expired record is not resolved, and its ID can be taken again by
    // another owner.
    assert!(resolve(store, &Query::kind("egress_proxy"), t0 + 10_001)
        .unwrap()
        .is_none());

    // A reaper that fails leaves it expired with why; one that succeeds
    // marks it reclaimed; a record without a reclaim is reclaimed at once.
    let failing = |_: &Service, _: &Reclaim| Outcome::Failed("boom".into());
    let reaped = reap(store, t0 + 10_001, &failing).unwrap();
    assert_eq!(reaped.len(), 1, "{label}");
    let row = store.get("px-c").unwrap().unwrap();
    assert_eq!(row.state, ServiceState::Expired, "{label}");
    assert_eq!(row.note.as_deref(), Some("reclaim failed: boom"), "{label}");
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let working = |s: &Service, r: &Reclaim| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(s.state, ServiceState::Reclaiming);
        assert!(matches!(r, Reclaim::Process { pid: 9, .. }));
        Outcome::Done("stopped pid 9".into())
    };
    reap(store, t0 + 10_002, &working).unwrap();
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "{label}"
    );
    let row = store.get("px-c").unwrap().unwrap();
    assert_eq!(row.state, ServiceState::Reclaimed, "{label}");
    assert_eq!(row.note.as_deref(), Some("stopped pid 9"), "{label}");
    // Reclaimed once: a second reap does not call the reaper.
    reap(store, t0 + 10_003, &working).unwrap();
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "{label}"
    );

    // A changed record is not transitioned from a stale read.
    let a = store.get("gw-a").unwrap().unwrap();
    store
        .renew("gw-a", "o-a", t0 + 30_000, None, t0 + 11)
        .unwrap()
        .unwrap();
    assert!(store
        .transition("gw-a", a.seq, ServiceState::Expired, None, t0 + 12)
        .unwrap()
        .is_none());

    // Deregistered by its owner: left, not resolved, never reaped.
    assert!(store.deregister("gw-b", "o-b", t0 + 20).unwrap(), "{label}");
    assert_eq!(
        store.get("gw-b").unwrap().unwrap().state,
        ServiceState::Left,
        "{label}"
    );
    assert_eq!(
        resolve(store, &github, t0 + 21).unwrap().unwrap().id,
        "gw-a",
        "{label}"
    );
    // A renewal of a record that left fails: its owner must register
    // again.
    assert!(store
        .renew("gw-b", "o-b", t0 + 50_000, None, t0 + 22)
        .unwrap()
        .is_none());

    // gw-a's lease runs out with nothing to reclaim.
    let never = |_: &Service, _: &Reclaim| -> Outcome { panic!("nothing to reclaim") };
    let reaped = reap(store, t0 + 30_000, &never).unwrap();
    assert_eq!(
        reaped
            .iter()
            .map(|r| r.service.id.as_str())
            .collect::<Vec<_>>(),
        ["gw-a"],
        "{label}"
    );
    assert_eq!(
        store.get("gw-a").unwrap().unwrap().state,
        ServiceState::Reclaimed,
        "{label}"
    );
    // Another owner may now take its ID.
    store
        .register(
            &gateway("gw-a", "o-y", &["github"], t0 + 90_000),
            t0 + 30_001,
        )
        .unwrap();

    // Ended records are pruned once older than KEEP_ENDED.
    let later = t0 + 30_002 + KEEP_ENDED.as_millis() as u64;
    reap(store, later, &never).ok();
    let ids: Vec<String> = store.all().unwrap().into_iter().map(|s| s.id).collect();
    assert!(!ids.contains(&"gw-b".to_owned()), "{label}: {ids:?}");
    assert!(!ids.contains(&"px-c".to_owned()), "{label}: {ids:?}");

    // Capabilities survive the round trip with their types.
    let typed = store
        .register(
            &{
                let mut s = Service::new("model_gateway", owner("o-m"))
                    .with_id("mg")
                    .with("models", vec!["m1".to_owned()])
                    .with("streaming", true)
                    .with("max_rps", 50_i64);
                s.lease_until_ms = later + 1_000;
                s
            },
            later,
        )
        .unwrap();
    let read = store.get("mg").unwrap().unwrap();
    assert_eq!(read.capabilities, typed.capabilities, "{label}");
    assert_eq!(
        read.capability("streaming"),
        Some(&Capability::Flag(true)),
        "{label}"
    );
}

/// Several handles (`stores`, one per thread, as separate processes or
/// connections would have) registering at once: every record lands, each
/// change number once.
pub fn check_concurrent(stores: Vec<Arc<dyn ServiceStore>>, each: usize, label: &str) {
    let barrier = Arc::new(Barrier::new(stores.len()));
    let threads: Vec<_> = stores
        .iter()
        .enumerate()
        .map(|(n, store)| {
            let (store, barrier) = (store.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                for i in 0..each {
                    let mut s = Service::new("worker", owner(&format!("c{n}")))
                        .with_id(format!("c{n}-{i}"));
                    s.lease_until_ms = u64::MAX / 2;
                    store.register(&s, 1).unwrap();
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let all = stores[0].all().unwrap();
    assert_eq!(all.len(), stores.len() * each, "{label}");
    let mut seqs: Vec<u64> = all.iter().map(|s| s.seq).collect();
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(seqs.len(), all.len(), "{label}: change numbers repeat");
}
