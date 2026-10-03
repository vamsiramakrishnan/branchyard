//! Sync end to end, between "machines" (separate repositories and
//! outboxes) through a directory and through the S3 stand-in: round
//! trips, racing writers, a crash between the objects and the manifest,
//! corruption, encryption and rotation, leases, garbage collection with
//! holds and retention, quotas, bounds on concurrency and bandwidth, and
//! incremental packs. Time comes from manual clocks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use branchyard::services::Clock;
use branchyard_sync::engine::{Options, Remote, Settings, TaskState};
use branchyard_sync::kms::{GcpKms, Passphrase};
use branchyard_sync::outbox::Outbox;
use branchyard_sync::pace::{ManualSleeper, RetryPolicy};
use branchyard_sync::replicator::{Replicator, SourceProvider};
use branchyard_sync::seal::{Algorithm, Encryption};
use branchyard_sync::source::{
    pointer_chunks, task_id_for_branch, write_chunk, BranchSource, ChunkId, Segment, SyncSource,
    POINTER_MAGIC,
};
use branchyard_sync::store::file::FileStore;
use branchyard_sync::store::{Entry, Generation, Object, ObjectStore, UploadJournal};
use branchyard_sync::testing::{commit, git, repo, FaultyStore, SlowStore};
use branchyard_sync::{Kind, Result};

const T0: u64 = 1_800_000_000_000;

struct World {
    _dir: tempfile::TempDir,
    root: PathBuf,
    clock: Clock,
    time: Arc<AtomicU64>,
    sleeper: Arc<ManualSleeper>,
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let (clock, time) = Clock::manual(T0);
    let sleeper = Arc::new(ManualSleeper::new(time.clone()));
    World {
        _dir: dir,
        root,
        clock,
        time,
        sleeper,
    }
}

impl World {
    fn advance(&self, by: Duration) {
        self.time.fetch_add(by.as_millis() as u64, Ordering::SeqCst);
    }

    fn file_store(&self) -> Arc<dyn ObjectStore> {
        Arc::new(
            FileStore::open(&self.root.join("bucket"))
                .unwrap()
                .with_part_size(4096),
        )
    }

    fn remote(&self, store: Arc<dyn ObjectStore>, device: &str) -> Remote {
        self.remote_with(store, device, Encryption::None, |_| {})
    }

    fn remote_with(
        &self,
        store: Arc<dyn ObjectStore>,
        device: &str,
        encryption: Encryption,
        tweak: impl FnOnce(&mut Options),
    ) -> Remote {
        let mut options = Options {
            encryption,
            settings: Settings {
                device: device.into(),
                retry: RetryPolicy {
                    attempts: 3,
                    base: Duration::from_millis(50),
                    cap: Duration::from_secs(1),
                    seed: 11,
                },
                ..Settings::default()
            },
            clock: self.clock.clone(),
            sleeper: self.sleeper.clone(),
            ..Options::default()
        };
        tweak(&mut options);
        Remote::open(store, options).unwrap()
    }

    /// A machine: a repository with one commit on `main`, and `feature`.
    fn machine(&self, name: &str) -> PathBuf {
        let dir = self.root.join(name);
        repo(&dir);
        dir
    }

    /// An empty repository, as a machine that has never seen the task.
    fn empty(&self, name: &str) -> PathBuf {
        let dir = self.root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "--quiet", "--initial-branch=main"]);
        dir
    }
}

fn source(dir: &Path, branch: &str) -> BranchSource {
    BranchSource::new(&dir.join(".git"), branch, branch).unwrap()
}

/// Commit on `branch` without leaving it checked out.
fn commit_on(dir: &Path, branch: &str, message: &str, files: &[(&str, &str)]) -> String {
    let exists = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .output()
        .unwrap()
        .status
        .success();
    match exists {
        true => git(dir, &["checkout", "--quiet", branch]),
        false => git(dir, &["checkout", "--quiet", "-b", branch]),
    };
    let oid = commit(dir, message, files);
    git(dir, &["checkout", "--quiet", "--detach"]);
    oid
}

fn head(dir: &Path, refname: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--verify", "--quiet", refname])
        .output()
        .unwrap();
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn round_trip(w: &World, store: Arc<dyn ObjectStore>) {
    let a = w.machine("a");
    let b = w.empty("b");
    let c1 = commit_on(&a, "feature", "one", &[("src/lib.rs", "fn one() {}\n")]);
    git(&a, &["update-ref", "refs/branchyard/feature/1/turn-1", &c1]);
    let ra = w.remote(store.clone(), "laptop");
    let rb = w.remote(store.clone(), "desktop");
    let (mut sa, mut sb) = (TaskState::default(), TaskState::default());

    let report = ra.sync(&source(&a, "feature"), &mut sa).unwrap();
    assert!(report.swapped);
    assert_eq!(report.seq, 1);
    assert_eq!(report.pushed.len(), 2, "{report:?}");

    // The other machine pulls it: refs, checkpoints and history.
    let report = rb.pull(&source(&b, "feature"), &mut sb).unwrap();
    assert_eq!(report.packs_down, 1);
    assert_eq!(head(&b, "refs/heads/feature").as_deref(), Some(c1.as_str()));
    assert_eq!(
        head(&b, "refs/branchyard/feature/1/turn-1").as_deref(),
        Some(c1.as_str())
    );
    assert_eq!(git(&b, &["show", "feature:src/lib.rs"]), "fn one() {}");

    // It works on, and syncs back: a fast-forward.
    let c2 = commit_on(&b, "feature", "two", &[("src/lib.rs", "fn two() {}\n")]);
    let report = rb.sync(&source(&b, "feature"), &mut sb).unwrap();
    assert_eq!(
        (report.seq, report.pushed.clone()),
        (2, vec!["refs/heads/main".to_owned()])
    );
    let report = ra.sync(&source(&a, "feature"), &mut sa).unwrap();
    assert!(!report.swapped, "nothing new to push");
    assert_eq!(report.pulled, vec!["refs/heads/main"]);
    assert_eq!(head(&a, "refs/heads/feature").as_deref(), Some(c2.as_str()));
    // Syncing again changes nothing.
    let report = ra.sync(&source(&a, "feature"), &mut sa).unwrap();
    assert!(!report.swapped && report.pulled.is_empty());
    assert_eq!(ra.tasks().unwrap()[0].task, "feature");
}

#[test]
fn a_task_round_trips_between_machines_through_a_directory() {
    let w = world();
    let store = w.file_store();
    round_trip(&w, store);
}

#[test]
fn a_task_round_trips_between_machines_through_s3() {
    let w = world();
    let s3 = branchyard_sync::testing::s3::MockS3::start("tasks", 5);
    round_trip(&w, Arc::new(s3.store("tasks", "team", 1 << 20)));
    assert!(s3
        .objects()
        .keys()
        .any(|k| k.starts_with("team/tasks/feature/manifest")));
}

/// Both writers read the manifest's generation before either writes it.
struct RaceStore {
    inner: Arc<dyn ObjectStore>,
    barrier: Barrier,
    armed: AtomicUsize,
}

impl ObjectStore for RaceStore {
    fn url(&self) -> String {
        self.inner.url()
    }
    fn get(&self, key: &str) -> Result<Object> {
        let out = self.inner.get(key);
        if key.ends_with("/manifest")
            && self
                .armed
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        {
            self.barrier.wait();
        }
        out
    }
    fn get_range(&self, key: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        self.inner.get_range(key, start, len)
    }
    fn stat(&self, key: &str) -> Result<Option<Entry>> {
        self.inner.stat(key)
    }
    fn put_if_absent(&self, key: &str, data: &[u8]) -> Result<Generation> {
        self.inner.put_if_absent(key, data)
    }
    fn put_if_match(&self, key: &str, data: &[u8], g: &str) -> Result<Generation> {
        self.inner.put_if_match(key, data, g)
    }
    fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        self.inner.list(prefix)
    }
    fn delete_if_match(&self, key: &str, g: &str) -> Result<()> {
        self.inner.delete_if_match(key, g)
    }
    fn resumable_put(&self, key: &str, data: &[u8], j: &dyn UploadJournal) -> Result<Generation> {
        self.inner.resumable_put(key, data, j)
    }
}

#[test]
fn racing_writers_one_swap_wins_the_other_merges_or_records_a_conflict() {
    let w = world();
    let base = w.file_store();
    let a = w.machine("a");
    let b = w.empty("b");
    commit_on(&a, "feature", "base", &[("f", "base\n")]);
    let (mut sa, mut sb) = (TaskState::default(), TaskState::default());
    w.remote(base.clone(), "a")
        .sync(&source(&a, "feature"), &mut sa)
        .unwrap();
    w.remote(base.clone(), "b")
        .pull(&source(&b, "feature"), &mut sb)
        .unwrap();

    // Divergent commits on main, and each machine a new attempt branch.
    let ca = commit_on(&a, "feature", "from a", &[("f", "a\n")]);
    let cb = commit_on(&b, "feature", "from b", &[("f", "b\n")]);
    let race = Arc::new(RaceStore {
        inner: base.clone(),
        barrier: Barrier::new(2),
        armed: AtomicUsize::new(2),
    });
    let ra = w.remote(race.clone(), "a");
    let rb = w.remote(race.clone(), "b");
    let (report_a, report_b) = std::thread::scope(|s| {
        let ta = s.spawn(|| ra.sync(&source(&a, "feature"), &mut sa).unwrap());
        let tb = s.spawn(|| rb.sync(&source(&b, "feature"), &mut sb).unwrap());
        (ta.join().unwrap(), tb.join().unwrap())
    });
    let conflicts = ra.stats().snapshot().swap_conflicts + rb.stats().snapshot().swap_conflicts;
    assert_eq!(conflicts, 1, "exactly one swap lost");
    let (winner, loser, loser_commit, loser_dev) = match report_a.rounds {
        1 => (report_a, report_b, cb.clone(), "b"),
        _ => (report_b, report_a, ca.clone(), "a"),
    };
    assert_eq!(winner.rounds, 1);
    assert_eq!(loser.rounds, 2);
    assert_eq!(loser.conflicts.len(), 1, "{loser:?}");
    let conflict = format!("refs/heads/conflict/{loser_dev}/1");
    let (manifest, _) = ra.manifest("feature").unwrap().unwrap();
    assert_eq!(manifest.seq, 3);
    assert_eq!(
        manifest.refs[&conflict], loser_commit,
        "the losing write is kept, as a branch"
    );
    assert_ne!(
        manifest.refs["refs/heads/main"], loser_commit,
        "never last-writer-wins"
    );
    // Syncing again records nothing new.
    let mut again = TaskState::default();
    let dir = if loser_dev == "a" { &a } else { &b };
    let r = w.remote(base.clone(), loser_dev);
    r.sync(&source(dir, "feature"), &mut again).unwrap();
    let r2 = r.sync(&source(dir, "feature"), &mut again).unwrap();
    assert!(r2.conflicts.is_empty() && !r2.swapped, "{r2:?}");
    // The person resolves by merging the conflict into main: a fast-forward
    // of the remote's main, pushed normally.
    let winner_dir = if loser_dev == "a" { &b } else { &a };
    let _ = winner;
    let mut sw = TaskState::default();
    let rw = w.remote(base.clone(), "w");
    rw.sync(&source(winner_dir, "feature"), &mut sw).unwrap();
    assert!(head(
        winner_dir,
        &format!("refs/heads/conflict/feature/{loser_dev}/1")
    )
    .is_some());

    // Two writers that touch different refs both land, merged.
    let a2 = commit_on(&a, "feature-x", "x", &[("x", "x\n")]);
    let _ = a2;
    let race = Arc::new(RaceStore {
        inner: base.clone(),
        barrier: Barrier::new(2),
        armed: AtomicUsize::new(2),
    });
    let mut s1 = TaskState::default();
    let mut s2 = TaskState::default();
    let r1 = w.remote(race.clone(), "a");
    let r2 = w.remote(race.clone(), "b");
    let ta = BranchSource::new(&a.join(".git"), "t2", "t2-a").unwrap();
    let tb = BranchSource::new(&b.join(".git"), "t2", "t2-a").unwrap();
    commit_on(&a, "t2-a", "a", &[("a", "1\n")]);
    git(&a, &["update-ref", "refs/branchyard/t2/1/turn-1", "HEAD"]);
    commit_on(&b, "t2-a", "b", &[("b", "1\n")]);
    git(&b, &["update-ref", "refs/branchyard/t2/2/turn-1", "HEAD"]);
    std::thread::scope(|s| {
        s.spawn(|| r1.sync(&ta, &mut s1).unwrap());
        s.spawn(|| r2.sync(&tb, &mut s2).unwrap());
    });
    let (m, _) = r1.manifest("t2").unwrap().unwrap();
    assert!(m.refs.contains_key("refs/branchyard/1/turn-1"));
    assert!(m.refs.contains_key("refs/branchyard/2/turn-1"));
}

#[test]
fn a_crash_between_objects_and_manifest_leaves_the_old_state_and_resumes() {
    let w = world();
    let faulty = Arc::new(FaultyStore::new(w.file_store()));
    let a = w.machine("a");
    let b = w.empty("b");
    let c1 = commit_on(&a, "feature", "one", &[("f", "1\n")]);
    let ra = w.remote(faulty.clone(), "a");
    let outbox = Arc::new(Outbox::open(&a.join(".branchyard/sync.db")).unwrap());
    struct One(PathBuf);
    impl SourceProvider for One {
        fn sources(&self) -> Result<Vec<Box<dyn SyncSource>>> {
            Ok(vec![Box::new(source(&self.0, "feature"))])
        }
        fn source(&self, _: &str) -> Result<Option<Box<dyn SyncSource>>> {
            Ok(Some(Box::new(source(&self.0, "feature"))))
        }
    }
    let replicator = Replicator::new(Arc::new(ra), outbox.clone(), Arc::new(One(a.clone())));
    assert_eq!(replicator.scan().unwrap(), vec!["feature"]);
    let drained = replicator.drain().unwrap();
    assert_eq!(drained.synced.len(), 1);

    // The next push dies after its objects, before its manifest.
    let c2 = commit_on(&a, "feature", "two", &[("f", "2\n")]);
    faulty.fail_writes_of(Some("/manifest"));
    assert_eq!(replicator.scan().unwrap(), vec!["feature"]);
    let drained = replicator.drain().unwrap();
    assert_eq!(drained.failed.len(), 1, "{drained:?}");
    let pending = outbox.pending(&replicator.remote().url()).unwrap();
    assert_eq!(pending.len(), 1, "the push stays in the outbox");
    assert!(pending[0]
        .last_error
        .as_deref()
        .unwrap()
        .contains("simulated crash"));
    assert!(pending[0].next_ms > w.clock.now(), "with a backoff");
    let packs = std::fs::read_dir(w.root.join("bucket/packs"))
        .unwrap()
        .count();
    assert_eq!(packs, 2, "the new pack went up");

    // The remote reads at the old state.
    let rb = w.remote(w.file_store(), "b");
    let mut sb = TaskState::default();
    rb.pull(&source(&b, "feature"), &mut sb).unwrap();
    assert_eq!(head(&b, "refs/heads/feature").as_deref(), Some(c1.as_str()));

    // The process restarts (a new outbox handle), the fault is gone, and
    // the push completes when due; the uploaded pack is reused.
    faulty.fail_writes_of(None);
    let outbox = Arc::new(Outbox::open(&a.join(".branchyard/sync.db")).unwrap());
    let replicator = Replicator::new(
        Arc::new(w.remote(faulty.clone(), "a")),
        outbox.clone(),
        Arc::new(One(a.clone())),
    );
    assert!(replicator.drain().unwrap().synced.is_empty(), "not due yet");
    w.advance(Duration::from_secs(3600));
    let drained = replicator.drain().unwrap();
    assert_eq!(drained.synced.len(), 1, "{drained:?}");
    assert!(outbox
        .pending(&replicator.remote().url())
        .unwrap()
        .is_empty());
    assert_eq!(
        std::fs::read_dir(w.root.join("bucket/packs"))
            .unwrap()
            .count(),
        2
    );
    rb.pull(&source(&b, "feature"), &mut sb).unwrap();
    assert_eq!(head(&b, "refs/heads/feature").as_deref(), Some(c2.as_str()));
    let status = replicator.status().unwrap();
    assert_eq!(status.tasks[0].state, "synced");
    assert!(status.counters.swaps >= 1);
    assert!(status.recent.iter().any(|e| e.outcome == "failed"));
}

#[test]
fn corrupted_objects_are_refused_and_scrub_finds_them() {
    let w = world();
    let store = w.file_store();
    let a = w.machine("a");
    let b = w.empty("b");
    commit_on(&a, "feature", "one", &[("f", "1\n")]);
    let ra = w.remote(store.clone(), "a");
    ra.sync(&source(&a, "feature"), &mut TaskState::default())
        .unwrap();
    let pack = std::fs::read_dir(w.root.join("bucket/packs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut bytes = std::fs::read(&pack).unwrap();
    let n = bytes.len();
    bytes[n - 30] ^= 0xff;
    std::fs::write(&pack, &bytes).unwrap();
    let rb = w.remote(store.clone(), "b");
    let e = rb
        .pull(&source(&b, "feature"), &mut TaskState::default())
        .unwrap_err();
    assert_eq!(e.kind, Kind::Corrupt, "{e}");
    assert!(
        head(&b, "refs/heads/feature").is_none(),
        "nothing half-imported"
    );
    assert_eq!(rb.stats().snapshot().corrupt, 1);
    let scrub = rb.scrub(100, 1, &[]).unwrap();
    assert_eq!(scrub.corrupt.len(), 1, "{scrub:?}");
    assert!(scrub.missing.is_empty());
}

fn chunked(dir: &Path, chunks: &Path, name: &str, bytes: &[u8]) -> ChunkId {
    let id = ChunkId::of(bytes);
    write_chunk(chunks, &id, bytes).unwrap();
    std::fs::write(
        dir.join(name),
        format!("{POINTER_MAGIC}\n{} {}\n", id.hex(), bytes.len()),
    )
    .unwrap();
    id
}

#[test]
fn chunks_and_segments_sync_and_scrub_repairs_a_chunk() {
    let w = world();
    let store = w.file_store();
    let a = w.machine("a");
    let b = w.empty("b");
    let (ca, cb) = (w.root.join("chunks-a"), w.root.join("chunks-b"));
    git(&a, &["checkout", "--quiet", "-b", "deck"]);
    let id = chunked(&a, &ca, "deck.pptx", b"slide bytes, large in real life");
    commit(&a, "deck", &[]);
    git(&a, &["checkout", "--quiet", "--detach"]);
    let seg = w.root.join("seg-0001.jsonl");
    std::fs::write(&seg, "{\"turn\":1}\n").unwrap();
    let src_a = BranchSource::new(&a.join(".git"), "deck", "deck")
        .unwrap()
        .with_chunks(&ca, pointer_chunks())
        .with_segments(
            vec![Segment {
                name: "conversation/0001.jsonl".into(),
                path: seg.clone(),
            }],
            &w.root.join("segments-a"),
        );
    let report = w
        .remote(store.clone(), "a")
        .sync(&src_a, &mut TaskState::default())
        .unwrap();
    assert_eq!((report.chunks_up, report.segments_up), (1, 1));
    let src_b = BranchSource::new(&b.join(".git"), "deck", "deck")
        .unwrap()
        .with_chunks(&cb, pointer_chunks())
        .with_segments(vec![], &w.root.join("segments-b"));
    let rb = w.remote(store.clone(), "b");
    let report = rb.pull(&src_b, &mut TaskState::default()).unwrap();
    assert_eq!((report.chunks_down, report.segments_down), (1, 1));
    assert_eq!(
        branchyard_sync::source::read_chunk(&cb, &id).unwrap(),
        b"slide bytes, large in real life"
    );
    assert_eq!(
        std::fs::read_to_string(w.root.join("segments-b/conversation/0001.jsonl")).unwrap(),
        "{\"turn\":1}\n"
    );
    // A corrupted chunk: refused, and repaired by scrub from a local copy.
    let key = format!("bucket/chunks/{}/{}", &id.hex()[..2], id.hex());
    let path = w.root.join(&key);
    let mut bytes = std::fs::read(&path).unwrap();
    let n = bytes.len();
    bytes[n - 1] ^= 1;
    std::fs::write(&path, &bytes).unwrap();
    let scrub = rb.scrub(1000, 7, std::slice::from_ref(&ca)).unwrap();
    assert_eq!(scrub.repaired.len(), 1, "{scrub:?}");
    assert!(scrub.clean());
    assert!(rb.scrub(1000, 8, &[]).unwrap().clean());
}

fn passphrase(p: &str) -> Encryption {
    Encryption::Envelope {
        wrapper: Box::new(Passphrase::new(p).unwrap().with_iterations(100)),
        algorithm: Algorithm::Aes256Gcm,
    }
}

/// Every file under `dir`, with its bytes.
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.insert(
                    p.strip_prefix(dir).unwrap().display().to_string(),
                    std::fs::read(&p).unwrap(),
                );
            }
        }
    }
    out
}

#[test]
fn an_encrypted_bucket_reveals_no_plaintext_names_or_hashes() {
    let w = world();
    let store = w.file_store();
    let a = w.machine("a");
    let b = w.empty("b");
    let chunks = w.root.join("chunks");
    git(&a, &["checkout", "--quiet", "-b", "secret-project"]);
    let id = chunked(&a, &chunks, "data.bin", b"TOP-SECRET-CHUNK-CONTENT");
    let c = commit(
        &a,
        "quarterly numbers",
        &[("notes.txt", "TOP-SECRET-NOTES\n")],
    );
    git(&a, &["checkout", "--quiet", "--detach"]);
    let src = BranchSource::new(&a.join(".git"), "secret-project", "secret-project")
        .unwrap()
        .with_chunks(&chunks, pointer_chunks());
    let ra = w.remote_with(
        store.clone(),
        "a",
        passphrase("correct horse battery"),
        |_| {},
    );
    ra.sync(&src, &mut TaskState::default()).unwrap();
    let bucket = files(&w.root.join("bucket"));
    let blob = git(&a, &["rev-parse", "secret-project:notes.txt"]);
    let pack_hash = {
        let (manifest, _) = ra.manifest("secret-project").unwrap().unwrap();
        manifest.packs[0].name.clone()
    };
    let secrets: Vec<String> = vec![
        "TOP-SECRET".into(),
        "secret-project".into(),
        "quarterly".into(),
        c.clone(),
        blob,
        id.hex(),
        "refs/heads".into(),
    ];
    for (name, bytes) in &bucket {
        let text = String::from_utf8_lossy(bytes);
        for s in &secrets {
            assert!(!name.contains(s.as_str()), "{name} reveals {s}");
            if name != "keyring.json" {
                assert!(!text.contains(s.as_str()), "{name} holds {s}");
            }
        }
    }
    // The pack's name is a keyed hash, not its content hash.
    let pack_bytes = &bucket[&format!("packs/{pack_hash}.pack")];
    assert!(!pack_bytes.starts_with(b"PACK"));
    // The right passphrase reads it all on another machine.
    let rb = w.remote_with(
        store.clone(),
        "b",
        passphrase("correct horse battery"),
        |_| {},
    );
    let src_b = BranchSource::new(&b.join(".git"), "secret-project", "secret-project")
        .unwrap()
        .with_chunks(&w.root.join("chunks-b"), pointer_chunks());
    rb.pull(&src_b, &mut TaskState::default()).unwrap();
    assert_eq!(
        head(&b, "refs/heads/secret-project").as_deref(),
        Some(c.as_str())
    );
    // The wrong one is refused before anything is read.
    let e = Remote::open(
        store.clone(),
        Options {
            encryption: passphrase("wrong horse battery"),
            ..Options::default()
        },
    )
    .err()
    .unwrap();
    assert_eq!(e.kind, Kind::Refused, "{e}");
    // A plain configuration is refused too.
    let e = Remote::open(store.clone(), Options::default())
        .err()
        .unwrap();
    assert_eq!(e.kind, Kind::Config);

    // Rotation: a new key version and passphrase; every data key rewrapped;
    // the old passphrase no longer opens it; names unchanged.
    let old = Passphrase::new("correct horse battery").unwrap();
    let new = Passphrase::new("a brand new passphrase")
        .unwrap()
        .with_iterations(100);
    let rotation = ra.rotate_key(&old, Some(&new)).unwrap();
    assert_eq!(rotation.new_version, 2);
    assert!(rotation.rewrapped >= 4, "{rotation:?}");
    // The version it superseded stays for the grace period (see the next test).
    assert!(rotation.retired.is_empty(), "{rotation:?}");
    let rc = w.remote_with(
        store.clone(),
        "c",
        passphrase("a brand new passphrase"),
        |_| {},
    );
    let c_dir = w.empty("c");
    rc.pull(&source(&c_dir, "secret-project"), &mut TaskState::default())
        .unwrap();
    assert_eq!(
        head(&c_dir, "refs/heads/secret-project").as_deref(),
        Some(c.as_str())
    );
    assert!(Remote::open(
        store.clone(),
        Options {
            encryption: passphrase("correct horse battery"),
            ..Options::default()
        }
    )
    .is_err());
    assert_eq!(
        files(&w.root.join("bucket"))
            .keys()
            .filter(|k| k.starts_with("packs/"))
            .count(),
        1,
        "the same objects, under the same names"
    );
}

#[test]
fn a_rotation_keeps_the_keys_writers_that_opened_before_it_still_use() {
    let w = world();
    let store = w.file_store();
    let a = w.machine("a");
    let b = w.machine("b");
    let key = || {
        Passphrase::new("one passphrase")
            .unwrap()
            .with_iterations(100)
    };
    let ra = w.remote_with(store.clone(), "a", passphrase("one passphrase"), |_| {});
    // Machine b opens the remote before the rotation: it seals under
    // version 1.
    let rb = w.remote_with(store.clone(), "b", passphrase("one passphrase"), |_| {});
    commit_on(&a, "first", "one", &[("f", "1\n")]);
    ra.sync(&source(&a, "first"), &mut TaskState::default())
        .unwrap();
    let rotation = ra.rotate_key(&key(), None).unwrap();
    assert_eq!(rotation.new_version, 2);
    assert!(rotation.retired.is_empty(), "{rotation:?}");
    // b uploads after the rotation's scan, still with version 1 (it read
    // the keyring under a minute ago); its manifest is published under the
    // keyring's current version, read just before the swap.
    let c2 = commit_on(&b, "second", "two", &[("g", "2\n")]);
    rb.sync(&source(&b, "second"), &mut TaskState::default())
        .unwrap();
    let sealed_under = |prefix: &str| -> Vec<u32> {
        files(&w.root.join("bucket"))
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .filter_map(|(_, v)| branchyard_sync::seal::Sealer::version_of(v))
            .collect()
    };
    assert!(sealed_under("packs/").contains(&1), "the late pack");
    assert_eq!(rb.sealer().current_version(), Some(2));
    // Everything b wrote stays readable to a machine opening it now.
    let rc = w.remote_with(store.clone(), "c", passphrase("one passphrase"), |_| {});
    let c = w.empty("c");
    rc.pull(&source(&c, "second"), &mut TaskState::default())
        .unwrap();
    assert_eq!(head(&c, "refs/heads/second").as_deref(), Some(c2.as_str()));
    // A rotation once the grace period has passed rewraps those objects,
    // then retires version 1 (and keeps 2, superseded only now).
    w.advance(ra.settings().grace + Duration::from_secs(1));
    let rotation = ra.rotate_key(&key(), None).unwrap();
    assert_eq!(rotation.new_version, 3);
    assert_eq!(rotation.retired, vec![1]);
    assert!(sealed_under("packs/").iter().all(|v| *v == 3));
    let rd = w.remote_with(store.clone(), "d", passphrase("one passphrase"), |_| {});
    let d = w.empty("d");
    for task in ["first", "second"] {
        rd.pull(&source(&d, task), &mut TaskState::default())
            .unwrap();
    }
    // b, which has not loaded version 3, reads the keyring again when it
    // meets it.
    rb.pull(&source(&b, "first"), &mut TaskState::default())
        .unwrap();
    assert_eq!(rb.sealer().current_version(), Some(3));
}

#[test]
fn a_kms_wrapped_remote_round_trips() {
    let w = world();
    let kms = branchyard_sync::testing::cloud::MockKms::start();
    let wrapper = || {
        GcpKms::new(
            "projects/p/locations/global/keyRings/r/cryptoKeys/k",
            Some(&kms.server.url),
            branchyard_sync::auth::google::GoogleAuth::fixed(
                branchyard_sync::testing::cloud::KMS_TOKEN,
            ),
        )
        .unwrap()
    };
    let store = w.file_store();
    let a = w.machine("a");
    let b = w.empty("b");
    let c = commit_on(&a, "feature", "one", &[("f", "1\n")]);
    let encryption = || Encryption::Envelope {
        wrapper: Box::new(wrapper()),
        algorithm: Algorithm::Chacha20Poly1305,
    };
    w.remote_with(store.clone(), "a", encryption(), |_| {})
        .sync(&source(&a, "feature"), &mut TaskState::default())
        .unwrap();
    w.remote_with(store.clone(), "b", encryption(), |_| {})
        .pull(&source(&b, "feature"), &mut TaskState::default())
        .unwrap();
    assert_eq!(head(&b, "refs/heads/feature").as_deref(), Some(c.as_str()));
    let calls = kms.calls();
    assert_eq!(
        calls.iter().filter(|c| *c == "gcp:encrypt").count(),
        1,
        "one key, wrapped once"
    );
    assert_eq!(calls.iter().filter(|c| *c == "gcp:decrypt").count(), 1);
}

#[test]
fn leases_are_exclusive_expire_and_are_registered() {
    let w = world();
    let store = w.file_store();
    let ra = Arc::new(w.remote(store.clone(), "a"));
    let rb = w.remote(store.clone(), "b");
    let ttl = Duration::from_secs(60);
    let mut la = ra
        .acquire_lease("feature", "attempt-1", "a:1", ttl)
        .unwrap();
    assert_eq!(la.epoch(), 1);
    let e = rb
        .acquire_lease("feature", "attempt-1", "b:1", ttl)
        .unwrap_err();
    assert_eq!(e.kind, Kind::LeaseHeld, "{e}");
    assert!(e.message.contains("a:1"));
    // Another attempt is free.
    rb.acquire_lease("feature", "attempt-2", "b:1", ttl)
        .unwrap();
    // Renewed, it lasts.
    w.advance(Duration::from_secs(50));
    ra.renew_lease(&mut la).unwrap();
    w.advance(Duration::from_secs(50));
    assert_eq!(
        rb.acquire_lease("feature", "attempt-1", "b:1", ttl)
            .unwrap_err()
            .kind,
        Kind::LeaseHeld
    );
    // Not renewed past its expiry and the allowed skew: taken over.
    w.advance(Duration::from_secs(60 + 30));
    let lb = rb
        .acquire_lease("feature", "attempt-1", "b:1", ttl)
        .unwrap();
    assert_eq!(lb.epoch(), 2, "a take-over bumps the epoch");
    // The old holder has lost it, and its release leaves the new lease.
    assert_eq!(ra.renew_lease(&mut la).unwrap_err().kind, Kind::LeaseHeld);
    ra.release_lease(la).unwrap();
    assert_eq!(
        rb.lease_state("feature", "attempt-1")
            .unwrap()
            .unwrap()
            .holder,
        "b:1"
    );
    rb.release_lease(lb).unwrap();
    assert!(rb.lease_state("feature", "attempt-1").unwrap().is_none());

    // A kept lease is registered in the service registry while held.
    let registry: Arc<dyn branchyard::services::ServiceStore> =
        Arc::new(branchyard::services::LocalRegistry::open(w.root.join("registry.db")).unwrap());
    let keeper = branchyard_sync::lease::LeaseKeeper::start(
        ra.clone(),
        "feature",
        "attempt-3",
        "a:2",
        ttl,
        Some(registry.clone()),
    )
    .unwrap();
    let live = registry.all().unwrap();
    let found = live
        .iter()
        .find(|s| s.kind == branchyard_sync::lease::KIND_SYNC_LEASE)
        .unwrap();
    assert_eq!(found.text("task"), Some("feature"));
    keeper.renew().unwrap();
    assert!(!keeper.lost());
    drop(keeper);
    assert!(
        ra.lease_state("feature", "attempt-3").unwrap().is_none(),
        "released when dropped"
    );
}

#[test]
fn gc_honours_the_grace_period_holds_and_retention() {
    let w = world();
    let faulty = Arc::new(FaultyStore::new(w.file_store()));
    let grace = Duration::from_secs(24 * 3600);
    let remote = |device: &str| {
        w.remote_with(faulty.clone(), device, Encryption::None, |o| {
            o.settings.grace = grace;
            o.settings.retention = Some(Duration::from_secs(30 * 86400));
        })
    };
    let a = w.machine("a");
    commit_on(&a, "feature", "one", &[("f", "1\n")]);
    let ra = remote("a");
    let mut sa = TaskState::default();
    ra.sync(&source(&a, "feature"), &mut sa).unwrap();

    // A sync that dies before its manifest leaves an unreferenced pack.
    commit_on(&a, "feature", "two", &[("f", "2\n")]);
    faulty.fail_writes_of(Some("/manifest"));
    assert!(ra.sync(&source(&a, "feature"), &mut sa).is_err());
    faulty.fail_writes_of(None);
    let report = ra.gc(false).unwrap();
    assert_eq!((report.unreferenced, report.in_grace), (1, 1), "{report:?}");
    assert!(report.deleted.is_empty());

    // Within the grace period, the writer comes back and swaps a manifest
    // that references it: it is never collected.
    w.advance(grace / 2);
    ra.sync(&source(&a, "feature"), &mut sa).unwrap();
    // And an orphan nobody will reference.
    let orphan = format!("packs/{}.pack", "ab".repeat(32));
    faulty.put_if_absent(&orphan, b"orphan").unwrap();
    let report = ra.gc(false).unwrap();
    assert_eq!(report.unreferenced, 1, "only the orphan: {report:?}");
    w.advance(grace / 2 + Duration::from_secs(1));
    let report = ra.gc(false).unwrap();
    assert!(
        report.deleted.is_empty(),
        "the orphan's grace started later: {report:?}"
    );
    w.advance(grace);
    let report = ra.gc(false).unwrap();
    assert_eq!(report.deleted, vec![orphan.clone()]);
    assert!(faulty.stat(&orphan).unwrap().is_none());
    assert_eq!(report.manifests, 1);
    let rb = w.remote(faulty.clone(), "b");
    let b = w.empty("b");
    rb.pull(&source(&b, "feature"), &mut TaskState::default())
        .unwrap();

    // A live writer defers the sweep.
    let mark = ra.begin_write().unwrap();
    let report = ra.gc(false).unwrap();
    assert!(report.deferred.is_some());
    ra.end_write(mark);

    // A second task on hold; retention expires the first, not the held one.
    commit_on(&a, "other", "o", &[("o", "1\n")]);
    ra.sync(&source(&a, "other"), &mut TaskState::default())
        .unwrap();
    ra.hold("other", "litigation 42", "legal@example.com")
        .unwrap();
    assert_eq!(ra.remove_task("other").unwrap_err().kind, Kind::Held);
    w.advance(Duration::from_secs(31 * 86400));
    let report = ra.gc(false).unwrap();
    assert_eq!(report.expired_tasks, vec!["feature"]);
    assert_eq!(report.held, vec!["other"]);
    assert!(ra.manifest("feature").unwrap().is_none());
    assert!(ra.manifest("other").unwrap().is_some());
    // The expired task's objects go after their grace; the held task's stay.
    w.advance(grace + Duration::from_secs(1));
    let report = ra.gc(false).unwrap();
    assert!(report.deleted.len() >= 2, "{report:?}");
    assert!(report.deleted.iter().all(|k| k.starts_with("packs/")));
    let c = w.empty("c");
    w.remote(faulty.clone(), "c")
        .pull(&source(&c, "other"), &mut TaskState::default())
        .unwrap();
    assert!(ra.release_hold("other").unwrap());
    assert!(ra.remove_task("other").unwrap());
    // Usage is recorded for quotas.
    assert!(report.usage.bytes > 0);
}

#[test]
fn quotas_refuse_a_push_that_would_exceed_them() {
    let w = world();
    let store = w.file_store();
    let a = w.machine("a");
    // Incompressible, so the pack is as large as the file.
    let big: String = (0..400u32)
        .map(|i| blake3::hash(&i.to_le_bytes()).to_hex().to_string())
        .collect();
    commit_on(&a, "feature", "big", &[("big.txt", &big)]);
    let ra = w.remote_with(store, "a", Encryption::None, |o| {
        o.settings.quota_bytes = Some(1_000);
    });
    let e = ra
        .sync(&source(&a, "feature"), &mut TaskState::default())
        .unwrap_err();
    assert_eq!(e.kind, Kind::Quota, "{e}");
    assert!(ra.manifest("feature").unwrap().is_none());
}

#[test]
fn quotas_count_what_was_written_since_the_collectors_count() {
    let w = world();
    let store = w.file_store();
    let a = w.machine("a");
    // Three tasks of one incompressible file each, about the same size.
    for (i, task) in ["t1", "t2", "t3"].iter().enumerate() {
        let big: String = (0..100u32)
            .map(|n| {
                blake3::hash(&(n + 1000 * i as u32).to_le_bytes())
                    .to_hex()
                    .to_string()
            })
            .collect();
        git(&a, &["branch", task, "main"]);
        commit_on(&a, task, task, &[("big.txt", &big)]);
    }
    // One is stored, and the collector counts it: `gc/usage` is fresh for
    // the next hour.
    let unlimited = w.remote(store.clone(), "a");
    let x = unlimited
        .sync(&source(&a, "t1"), &mut TaskState::default())
        .unwrap()
        .pack
        .unwrap()
        .bytes;
    assert_eq!(unlimited.gc(false).unwrap().usage.bytes, x);
    // Room for one more and a half: each sync alone fits under the count,
    // but the second must see the first.
    let ra = w.remote_with(store.clone(), "a", Encryption::None, |o| {
        o.settings.quota_bytes = Some(x * 5 / 2);
    });
    ra.sync(&source(&a, "t2"), &mut TaskState::default())
        .unwrap();
    assert!(
        ra.usage().unwrap() >= 2 * x - x / 50,
        "counted since the count"
    );
    let e = ra
        .sync(&source(&a, "t3"), &mut TaskState::default())
        .unwrap_err();
    assert_eq!(e.kind, Kind::Quota, "{e}");
    assert!(ra.manifest("t3").unwrap().is_none());
    // Another machine knows nothing of those writes; near the quota it
    // counts the bucket instead of trusting the collector's count.
    let rb = w.remote_with(store.clone(), "b", Encryption::None, |o| {
        o.settings.quota_bytes = Some(x * 21 / 10);
    });
    let e = rb
        .sync(&source(&a, "t3"), &mut TaskState::default())
        .unwrap_err();
    assert_eq!(e.kind, Kind::Quota, "{e}");
    assert!(rb.manifest("t3").unwrap().is_none());
}

#[test]
fn uploads_are_bounded_in_concurrency_and_bandwidth() {
    let w = world();
    let slow = Arc::new(SlowStore::new(w.file_store(), "chunks/", 3));
    let a = w.machine("a");
    let chunks = w.root.join("chunks");
    git(&a, &["checkout", "--quiet", "-b", "many"]);
    let mut pointer = format!("{POINTER_MAGIC}\n");
    let mut total = 0;
    for i in 0..12 {
        let bytes = vec![i as u8; 10_000];
        total += bytes.len();
        let id = ChunkId::of(&bytes);
        write_chunk(&chunks, &id, &bytes).unwrap();
        pointer.push_str(&format!("{} {}\n", id.hex(), bytes.len()));
    }
    commit(&a, "many", &[("files.ptr", &pointer)]);
    git(&a, &["checkout", "--quiet", "--detach"]);
    let src = BranchSource::new(&a.join(".git"), "many", "many")
        .unwrap()
        .with_chunks(&chunks, pointer_chunks());
    let ra = w.remote_with(slow.clone(), "a", Encryption::None, |o| {
        o.settings.concurrency = 3;
        o.bandwidth = Some(50_000);
    });
    let before = w.sleeper.slept_ms.load(Ordering::SeqCst);
    let report = ra.sync(&src, &mut TaskState::default()).unwrap();
    assert_eq!(report.chunks_up, 12);
    assert_eq!(
        slow.max.load(Ordering::SeqCst),
        3,
        "three in flight at once, never more"
    );
    assert!(ra.max_in_flight() <= 3 + 1);
    // ~120 KB of chunks plus the pack and manifest at 50 KB/s: at least
    // two seconds of waiting beyond the first second's burst.
    let slept = w.sleeper.slept_ms.load(Ordering::SeqCst) - before;
    let floor = ((total as u64).saturating_sub(50_000)) * 1000 / 50_000;
    assert!(slept >= floor, "slept {slept} ms, want at least {floor}");
}

#[test]
fn packs_are_incremental() {
    let w = world();
    let store = w.file_store();
    let a = w.machine("a");
    commit_on(
        &a,
        "feature",
        "one",
        &[("a.txt", "a\n"), ("dir/b.txt", "b\n")],
    );
    let ra = w.remote(store, "a");
    let mut sa = TaskState::default();
    let first = ra
        .sync(&source(&a, "feature"), &mut sa)
        .unwrap()
        .pack
        .unwrap();
    assert!(first.objects >= 6, "{first:?}");
    commit_on(&a, "feature", "two", &[("a.txt", "changed\n")]);
    let second = ra
        .sync(&source(&a, "feature"), &mut sa)
        .unwrap()
        .pack
        .unwrap();
    assert_eq!(
        second.objects, 3,
        "a commit, its tree and one blob: {second:?}"
    );
    // A sync with nothing new uploads nothing.
    let third = ra.sync(&source(&a, "feature"), &mut sa).unwrap();
    assert!(third.pack.is_none() && !third.swapped);
    let (manifest, _) = ra.manifest("feature").unwrap().unwrap();
    assert_eq!(manifest.packs.len(), 2);
    assert_eq!(task_id_for_branch("by/fix-1"), "by~fix-1");
}

#[test]
fn a_checked_out_ref_moves_only_when_its_worktree_is_clean() {
    let w = world();
    let store = w.file_store();
    let a = w.machine("a");
    let b = w.empty("b");
    let c1 = commit_on(&a, "feature", "one", &[("f", "1\n")]);
    let ra = w.remote(store.clone(), "a");
    let rb = w.remote(store.clone(), "b");
    let mut sa = TaskState::default();
    ra.sync(&source(&a, "feature"), &mut sa).unwrap();
    rb.pull(&source(&b, "feature"), &mut TaskState::default())
        .unwrap();
    git(&a, &["checkout", "--quiet", "feature"]);
    std::fs::write(a.join("f"), "local edit\n").unwrap();
    let c2 = commit_on(&b, "feature", "two", &[("f", "2\n")]);
    rb.sync(&source(&b, "feature"), &mut TaskState::default())
        .unwrap();
    let report = ra.sync(&source(&a, "feature"), &mut sa).unwrap();
    assert_eq!(report.behind, vec!["refs/heads/main"]);
    assert_eq!(head(&a, "refs/heads/feature").as_deref(), Some(c1.as_str()));
    assert_eq!(
        std::fs::read_to_string(a.join("f")).unwrap(),
        "local edit\n"
    );
    // A clean worktree follows the fast-forward.
    git(&a, &["checkout", "--quiet", "--", "f"]);
    let report = ra.sync(&source(&a, "feature"), &mut sa).unwrap();
    assert!(report.behind.is_empty(), "{report:?}");
    assert_eq!(head(&a, "refs/heads/feature").as_deref(), Some(c2.as_str()));
    assert_eq!(std::fs::read_to_string(a.join("f")).unwrap(), "2\n");
}

#[test]
fn a_remote_deletion_of_a_checked_out_ref_is_not_pushed_back() {
    let w = world();
    let store = w.file_store();
    let a = w.machine("a");
    let b = w.empty("b");
    let c1 = commit_on(&a, "feature", "one", &[("f", "1\n")]);
    let ra = w.remote(store.clone(), "a");
    let rb = w.remote(store.clone(), "b");
    let (mut sa, mut sb) = (TaskState::default(), TaskState::default());
    ra.sync(&source(&a, "feature"), &mut sa).unwrap();
    rb.sync(&source(&b, "feature"), &mut sb).unwrap();
    git(&a, &["checkout", "--quiet", "feature"]);
    // The other machine deletes the branch's head, and the remote follows.
    git(&b, &["update-ref", "-d", "refs/heads/feature"]);
    let report = rb.sync(&source(&b, "feature"), &mut sb).unwrap();
    assert_eq!(report.deleted, vec!["refs/heads/main"]);
    let deleted_at = ra.manifest("feature").unwrap().unwrap().0.seq;
    // Here it is checked out: left, and reported behind (the manifest is
    // unchanged, so nothing is swapped).
    let report = ra.sync(&source(&a, "feature"), &mut sa).unwrap();
    assert_eq!(report.behind, vec!["refs/heads/main"]);
    assert!(!report.swapped, "{report:?}");
    assert_eq!(head(&a, "refs/heads/feature").as_deref(), Some(c1.as_str()));
    // The next sync must not take the surviving ref for a new one and push
    // it back.
    let report = ra.sync(&source(&a, "feature"), &mut sa).unwrap();
    assert!(report.pushed.is_empty(), "{report:?}");
    assert_eq!(report.behind, vec!["refs/heads/main"]);
    let (m, _) = ra.manifest("feature").unwrap().unwrap();
    assert!(!m.refs.contains_key("refs/heads/main"), "{m:?}");
    assert_eq!(m.seq, deleted_at);
    // Nor when the same sync swaps the manifest for another change.
    git(&a, &["update-ref", "refs/branchyard/feature/1/turn-1", &c1]);
    let report = ra.sync(&source(&a, "feature"), &mut sa).unwrap();
    assert!(report.swapped, "{report:?}");
    assert_eq!(report.pushed, vec!["refs/branchyard/1/turn-1"]);
    let report = ra.sync(&source(&a, "feature"), &mut sa).unwrap();
    assert!(report.pushed.is_empty(), "{report:?}");
    let (m, _) = ra.manifest("feature").unwrap().unwrap();
    assert!(!m.refs.contains_key("refs/heads/main"), "{m:?}");
    // Once the worktree leaves the branch, the deletion is applied here.
    git(&a, &["checkout", "--quiet", "--detach"]);
    let report = ra.sync(&source(&a, "feature"), &mut sa).unwrap();
    assert!(report.behind.is_empty(), "{report:?}");
    assert_eq!(head(&a, "refs/heads/feature"), None);
}

#[test]
fn transient_failures_are_retried_with_backoff() {
    let w = world();
    let s3 = branchyard_sync::testing::s3::MockS3::start("b", 50);
    let a = w.machine("a");
    commit_on(&a, "feature", "one", &[("f", "1\n")]);
    s3.faults.fail("PUT", "/manifest", 503, 0, 2);
    let ra = w.remote(Arc::new(s3.store("b", "", 1 << 20)), "a");
    let before = w.sleeper.slept_ms.load(Ordering::SeqCst);
    ra.sync(&source(&a, "feature"), &mut TaskState::default())
        .unwrap();
    assert_eq!(ra.stats().snapshot().retries, 2);
    assert!(w.sleeper.slept_ms.load(Ordering::SeqCst) >= before);
    let manifest_puts = s3
        .server
        .requests()
        .iter()
        .filter(|l| l.starts_with("PUT") && l.contains("/manifest"))
        .count();
    assert_eq!(manifest_puts, 3);
}
