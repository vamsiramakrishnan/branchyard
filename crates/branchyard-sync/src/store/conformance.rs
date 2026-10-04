//! The semantics every backend must have, as one test: run [`check`]
//! against a store (the directory backend in every test run; the
//! stand-ins for S3, GCS and Azure; a git remote; and MinIO,
//! fake-gcs-server or Azurite when they are configured). It panics on the
//! first difference. Keys go under a fresh random prefix, so it can run
//! against a shared bucket.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::error::Kind;
use crate::store::{MemoryJournal, ObjectStore};

/// What a backend is checked with.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// The size of the object the resumable upload writes: larger than the
    /// store's part size, so it goes up in parts.
    pub large: usize,
    /// Whether the backend reports sizes (the git backend does not).
    pub sizes: bool,
}

/// Run every check against `store`.
pub fn check(store: Arc<dyn ObjectStore>, options: Options) {
    let large = options.large;
    let run = crate::util::hex(&crate::util::random_bytes(6).unwrap());
    let p = format!("conformance-{run}");
    let key = |k: &str| format!("{p}/{k}");
    let s = store.as_ref();

    // Missing objects.
    let e = s.get(&key("missing")).unwrap_err();
    assert_eq!(e.kind, Kind::NotFound, "get of a missing object: {e}");
    assert!(s.stat(&key("missing")).unwrap().is_none());

    // Create only if absent.
    let g1 = s.put_if_absent(&key("a"), b"hello world").unwrap();
    let e = s.put_if_absent(&key("a"), b"other").unwrap_err();
    assert_eq!(e.kind, Kind::Precondition, "a second put_if_absent: {e}");
    let object = s.get(&key("a")).unwrap();
    assert_eq!(object.data, b"hello world");
    assert_eq!(object.generation, g1);
    let stat = s.stat(&key("a")).unwrap().unwrap();
    assert_eq!(stat.generation, g1);
    if options.sizes {
        assert_eq!(stat.size, 11);
    }

    // Ranges.
    assert_eq!(s.get_range(&key("a"), 6, 5).unwrap(), b"world");
    assert_eq!(s.get_range(&key("a"), 6, 100).unwrap(), b"world");
    assert_eq!(s.get_range(&key("a"), 0, 0).unwrap(), b"");
    assert!(s.get_range(&key("a"), 50, 4).unwrap().is_empty());

    // Replace only the generation named.
    let e = s
        .put_if_match(&key("a"), b"x", "not-a-generation")
        .unwrap_err();
    assert_eq!(
        e.kind,
        Kind::Precondition,
        "put_if_match, wrong generation: {e}"
    );
    let g2 = s.put_if_match(&key("a"), b"hello again", &g1).unwrap();
    assert_ne!(g1, g2, "a replacement has a new generation");
    assert_eq!(s.get(&key("a")).unwrap().data, b"hello again");
    let e = s.put_if_match(&key("a"), b"stale", &g1).unwrap_err();
    assert_eq!(
        e.kind,
        Kind::Precondition,
        "put_if_match, stale generation: {e}"
    );
    let e = s.put_if_match(&key("nothing"), b"x", &g2).unwrap_err();
    assert_eq!(
        e.kind,
        Kind::Precondition,
        "put_if_match of a missing object: {e}"
    );

    // Empty objects.
    s.put_if_absent(&key("empty"), b"").unwrap();
    assert_eq!(s.get(&key("empty")).unwrap().data, b"");

    // Listing: sorted, by string prefix, across pages.
    for i in 0..7 {
        s.put_if_absent(&key(&format!("list/{i}")), format!("{i}").as_bytes())
            .unwrap();
    }
    s.put_if_absent(&key("list-other"), b"x").unwrap();
    s.put_if_absent(&key("listing/deep/x"), b"x").unwrap();
    let listed: Vec<String> = s
        .list(&key("list/"))
        .unwrap()
        .into_iter()
        .map(|e| e.key)
        .collect();
    let want: Vec<String> = (0..7).map(|i| key(&format!("list/{i}"))).collect();
    assert_eq!(listed, want);
    let listed: Vec<String> = s
        .list(&key("list"))
        .unwrap()
        .into_iter()
        .map(|e| e.key)
        .collect();
    assert_eq!(listed.len(), 9, "{listed:?}");
    let entry = &s.list(&key("list/3")).unwrap()[0];
    if options.sizes {
        assert_eq!(entry.size, 1);
    }

    // Delete only the generation named.
    let e = s.delete_if_match(&key("a"), &g1).unwrap_err();
    assert_eq!(e.kind, Kind::Precondition, "delete_if_match, stale: {e}");
    s.delete_if_match(&key("a"), &g2).unwrap();
    assert_eq!(s.get(&key("a")).unwrap_err().kind, Kind::NotFound);
    let e = s.delete_if_match(&key("a"), &g2).unwrap_err();
    assert_eq!(e.kind, Kind::Precondition, "deleting it twice: {e}");
    // A deleted key can be created again.
    s.put_if_absent(&key("a"), b"reborn").unwrap();

    // Resumable uploads: put_if_absent in parts.
    let data: Vec<u8> = (0..large).map(|i| (i * 7 % 251) as u8).collect();
    let journal = MemoryJournal::default();
    let g = s.resumable_put(&key("large"), &data, &journal).unwrap();
    let object = s.get(&key("large")).unwrap();
    assert!(object.data == data, "the large object reads back whole");
    assert_eq!(object.generation, g);
    assert!(
        journal.entries().is_empty(),
        "a finished upload clears its journal"
    );
    let e = s.resumable_put(&key("large"), &data, &journal).unwrap_err();
    assert_eq!(
        e.kind,
        Kind::Precondition,
        "resumable_put of an existing key: {e}"
    );

    // Concurrent writers: exactly one creates, exactly one swaps.
    let wins = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for i in 0..6 {
            let (store, wins, k) = (store.clone(), wins.clone(), key("race"));
            scope.spawn(move || {
                if store.put_if_absent(&k, format!("{i}").as_bytes()).is_ok() {
                    wins.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });
    assert_eq!(wins.load(Ordering::SeqCst), 1, "one put_if_absent wins");
    let base = s.get(&key("race")).unwrap().generation;
    let wins = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for i in 0..6 {
            let (store, wins, k, base) = (store.clone(), wins.clone(), key("race"), base.clone());
            scope.spawn(move || {
                if store
                    .put_if_match(&k, format!("swap {i}").as_bytes(), &base)
                    .is_ok()
                {
                    wins.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });
    assert_eq!(wins.load(Ordering::SeqCst), 1, "one put_if_match wins");

    // Clean up what this run made.
    for entry in s.list(&p).unwrap() {
        branchyard_support::best_effort(
            "s.delete_if_match",
            s.delete_if_match(&entry.key, &entry.generation),
        );
    }
}
