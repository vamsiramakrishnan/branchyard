//! Artifacts and scratch areas against the fake ACP agent: reads follow the
//! delegation tree, siblings need an explicit share, GC after removal, and
//! a scratch area's single writer enforced across two processes (two
//! [`Yard`] handles on one repository, as other durability tests use to
//! stand in for two engines).

mod common;

use std::collections::BTreeMap;
use std::fs;

use branchyard::Error;
use common::{edit_record, text, Fixture};

/// Give `name`'s branch `parent` (as a delegated child would have) and
/// leave its status `Ready`, so it is authorized by ancestry but not
/// counted as a running turn.
fn make_child(f: &Fixture, name: &str, parent: &str) {
    edit_record(&f.root, name, |record| {
        record["info"]["parent"] = parent.into();
    });
}

fn set_running(f: &Fixture, name: &str, running: bool) {
    edit_record(&f.root, name, |record| {
        record["info"]["status"] = if running {
            serde_json::json!({"state": "running"})
        } else {
            serde_json::json!({"state": "ready"})
        };
    });
}

#[test]
fn published_artifacts_follow_the_delegation_tree() {
    let f = Fixture::new();
    for name in ["root", "a", "b"] {
        f.task("do nothing").name(name).run().unwrap();
    }
    make_child(&f, "a", "root");
    make_child(&f, "b", "root");

    let file = f.dir.join("payload.txt");
    fs::write(&file, b"hello artifact").unwrap();
    let published = f
        .yard
        .publish_artifact(
            "a",
            &file,
            Some("payload.txt".into()),
            None,
            BTreeMap::from([("kind".into(), "test".into())]),
        )
        .unwrap();
    assert_eq!(published.size, 14);
    assert_eq!(published.publisher_branch, "a");
    assert!(!published.digest.is_empty());

    // The publisher and its ancestor read it.
    assert!(f
        .yard
        .artifacts("a")
        .unwrap()
        .iter()
        .any(|a| a.id == published.id));
    assert!(f
        .yard
        .artifacts("root")
        .unwrap()
        .iter()
        .any(|a| a.id == published.id));
    // A sibling does not, until shared.
    assert!(!f
        .yard
        .artifacts("b")
        .unwrap()
        .iter()
        .any(|a| a.id == published.id));
    assert!(matches!(
        f.yard
            .read_artifact("b", &published.id, f.dir.join("out.txt")),
        Err(Error::Denied(_))
    ));
    f.yard.share_artifact("a", &published.id, "b").unwrap();
    assert!(f
        .yard
        .artifacts("b")
        .unwrap()
        .iter()
        .any(|a| a.id == published.id));

    let out = f.dir.join("out.txt");
    let read = f.yard.read_artifact("b", &published.id, &out).unwrap();
    assert_eq!(read, published);
    assert_eq!(fs::read(&out).unwrap(), b"hello artifact");
}

#[test]
fn removing_an_unreferenced_publisher_gcs_its_artifact() {
    let f = Fixture::new();
    f.task("do nothing").name("orphan").run().unwrap();
    let file = f.dir.join("payload.txt");
    fs::write(&file, b"gc me").unwrap();
    let published = f
        .yard
        .publish_artifact("orphan", &file, None, None, BTreeMap::new())
        .unwrap();
    let blob = f
        .root
        .join(".branchyard/artifacts")
        .join(&published.digest[..2])
        .join(&published.digest);
    assert!(blob.is_file());

    f.yard.remove("orphan").unwrap();

    assert!(
        f.yard
            .read_artifact("orphan", &published.id, f.dir.join("x"))
            .is_err(),
        "orphan no longer exists to act as a reader"
    );
    assert!(!blob.exists(), "the unreferenced blob was collected");
}

#[test]
fn an_ancestors_read_survives_the_publishers_removal() {
    let f = Fixture::new();
    for name in ["root", "a"] {
        f.task("do nothing").name(name).run().unwrap();
    }
    make_child(&f, "a", "root");
    let file = f.dir.join("payload.txt");
    fs::write(&file, b"kept").unwrap();
    let published = f
        .yard
        .publish_artifact("a", &file, None, None, BTreeMap::new())
        .unwrap();

    f.yard.remove("a").unwrap();

    // root is an ancestor of the now-removed publisher, so the artifact is
    // still referenced and was not collected; root can still read it.
    let out = f.dir.join("out.txt");
    let read = f.yard.read_artifact("root", &published.id, &out).unwrap();
    assert_eq!(read.id, published.id);
    assert_eq!(fs::read(&out).unwrap(), b"kept");
}

#[test]
fn scratch_areas_need_an_explicit_share_for_a_sibling() {
    let f = Fixture::new();
    for name in ["root", "a", "b"] {
        f.task("do nothing").name(name).run().unwrap();
    }
    make_child(&f, "a", "root");
    make_child(&f, "b", "root");

    let area = f.yard.create_scratch("a", "cache").unwrap();
    assert_eq!(area.owner_branch, "a");
    assert!(f.yard.scratch_path("cache").is_dir());

    assert!(f
        .yard
        .scratch_areas("root")
        .unwrap()
        .iter()
        .any(|s| s.name == "cache"));
    assert!(!f
        .yard
        .scratch_areas("b")
        .unwrap()
        .iter()
        .any(|s| s.name == "cache"));
    assert!(matches!(
        f.yard.lock_scratch("b", "cache"),
        Err(Error::Denied(_))
    ));

    f.yard.share_scratch("a", "cache", "b").unwrap();
    let lock = f.yard.lock_scratch("b", "cache").unwrap();
    assert_eq!(lock.holder_branch, "b");
}

#[test]
fn a_scratch_lock_is_one_writer_at_a_time_across_two_processes() {
    let f = Fixture::new();
    for name in ["a", "b"] {
        f.task("do nothing").name(name).run().unwrap();
    }
    // Two independent engine handles on the same repository, standing in
    // for two processes (as other durability tests do).
    let one = branchyard::Yard::open(&f.root).unwrap();
    let two = branchyard::Yard::open(&f.root).unwrap();

    one.create_scratch("a", "shared").unwrap();
    one.share_scratch("a", "shared", "b").unwrap();

    let lock = one.lock_scratch("a", "shared").unwrap();
    assert_eq!(lock.holder_branch, "a");
    // a's turn is (simulated) still running: b, from the other process,
    // is refused the lock rather than granted it alongside a.
    set_running(&f, "a", true);
    assert!(matches!(
        two.lock_scratch("b", "shared"),
        Err(Error::Running(_))
    ));
    // Re-entrant for a itself, from either handle.
    assert_eq!(two.lock_scratch("a", "shared").unwrap().holder_branch, "a");

    // a's turn ends: the lock is reclaimed for b, from the other process,
    // without a having called unlock.
    set_running(&f, "a", false);
    let lock = two.lock_scratch("b", "shared").unwrap();
    assert_eq!(lock.holder_branch, "b");
    assert_eq!(
        one.scratch_lock_state("shared")
            .unwrap()
            .unwrap()
            .holder_branch,
        "b"
    );

    two.unlock_scratch("b", "shared").unwrap();
    assert!(one.scratch_lock_state("shared").unwrap().is_none());
}

#[test]
fn a_turn_gets_its_authorized_scratch_areas_as_environment_variables() {
    let f = Fixture::new();
    // Created before the branch's first turn, so it is authorized (a
    // branch always reads its own scratch areas) from that very turn.
    f.task("do nothing").name("root").run().unwrap();
    let area = f.yard.create_scratch("root", "shared-cache").unwrap();
    assert_eq!(area.name, "shared-cache");

    let branch = f
        .yard
        .branch("root")
        .unwrap()
        .send("ENV BRANCHYARD_SCRATCH_SHARED_CACHE", f.options())
        .unwrap();
    let seen = text(&branch.events().unwrap());
    let expected = f.yard.scratch_path("shared-cache");
    assert!(
        seen.contains(&expected.display().to_string()),
        "expected {expected:?} in harness output: {seen:?}"
    );
}

#[test]
fn scratch_path_matches_authorized_area() {
    let f = Fixture::new();
    f.task("do nothing").name("root").run().unwrap();
    f.yard.create_scratch("root", "shared-cache").unwrap();
    let areas = f.yard.scratch_areas("root").unwrap();
    assert_eq!(areas.len(), 1);
    assert_eq!(
        f.yard.scratch_path("shared-cache"),
        f.root.join(".branchyard/scratch/shared-cache")
    );
}

/// Review finding: grants were bound to branch names, and a name is reusable
/// once its branch is removed. A new, unrelated branch that takes a removed
/// publisher's name, a removed share target's name or a removed scratch
/// owner's name must not inherit their access.
#[test]
fn a_reused_branch_name_inherits_no_grant() {
    let f = Fixture::new();
    for name in ["root", "kid", "friend"] {
        f.task("do nothing").name(name).run().unwrap();
    }
    make_child(&f, "kid", "root");
    let file = f.dir.join("payload.txt");
    fs::write(&file, b"kid's work").unwrap();
    let by_kid = f
        .yard
        .publish_artifact("kid", &file, None, None, BTreeMap::new())
        .unwrap();
    let by_root = f
        .yard
        .publish_artifact("root", &file, None, None, BTreeMap::new())
        .unwrap();
    f.yard
        .share_artifact("root", &by_root.id, "friend")
        .unwrap();
    f.yard.create_scratch("kid", "kid-cache").unwrap();
    assert!(f
        .yard
        .artifacts("friend")
        .unwrap()
        .iter()
        .any(|a| a.id == by_root.id));

    // kid and friend are removed; root (kid's ancestor) keeps kid's
    // artifact and scratch area alive.
    f.yard.remove("kid").unwrap();
    f.yard.remove("friend").unwrap();
    assert!(f
        .yard
        .artifacts("root")
        .unwrap()
        .iter()
        .any(|a| a.id == by_kid.id));

    // Unrelated branches take both names.
    for name in ["kid", "friend"] {
        f.task("do nothing").name(name).run().unwrap();
    }
    let out = f.dir.join("out.txt");
    assert!(
        matches!(
            f.yard.read_artifact("kid", &by_kid.id, &out),
            Err(Error::Denied(_))
        ),
        "a new branch named like the removed publisher read its artifact"
    );
    assert!(!f
        .yard
        .artifacts("kid")
        .unwrap()
        .iter()
        .any(|a| a.id == by_kid.id));
    assert!(
        matches!(
            f.yard.read_artifact("friend", &by_root.id, &out),
            Err(Error::Denied(_))
        ),
        "a new branch named like a removed share target read the shared artifact"
    );
    assert!(!f
        .yard
        .scratch_areas("kid")
        .unwrap()
        .iter()
        .any(|s| s.name == "kid-cache"));
    assert!(matches!(
        f.yard.lock_scratch("kid", "kid-cache"),
        Err(Error::Denied(_))
    ));
    // The rightful reader still reads.
    f.yard.read_artifact("root", &by_kid.id, &out).unwrap();
}

/// Review finding: collection only re-examined what the removed branch
/// itself published, so an artifact kept alive by an ancestor was never
/// collected once that ancestor went too.
#[test]
fn a_grandchilds_artifact_is_collected_after_its_last_reader_is_removed() {
    let f = Fixture::new();
    for name in ["root", "a", "aa"] {
        f.task("do nothing").name(name).run().unwrap();
    }
    make_child(&f, "a", "root");
    make_child(&f, "aa", "a");
    let file = f.dir.join("payload.txt");
    fs::write(&file, b"grandchild output").unwrap();
    let published = f
        .yard
        .publish_artifact("aa", &file, None, None, BTreeMap::new())
        .unwrap();
    f.yard.create_scratch("aa", "deep").unwrap();
    let blob = f
        .root
        .join(".branchyard/artifacts")
        .join(&published.digest[..2])
        .join(&published.digest);

    f.yard.remove("aa").unwrap();
    f.yard.remove("a").unwrap();
    // root, an ancestor captured at publish time, still reads it.
    assert!(blob.is_file());
    f.yard
        .read_artifact("root", &published.id, f.dir.join("out.txt"))
        .unwrap();

    f.yard.remove("root").unwrap();
    assert!(
        !blob.exists(),
        "the last reader is gone: the blob is collected"
    );
    assert!(
        !f.yard.scratch_path("deep").exists(),
        "the last reader is gone: the scratch area is collected"
    );
    f.task("do nothing").name("root").run().unwrap();
    assert!(f.yard.artifacts("root").unwrap().is_empty());
}

/// Review finding: the digest was computed on one open of the path and the
/// bytes copied from a second open, so bytes changed in between were stored
/// under the wrong digest. A FIFO makes the two opens see different bytes
/// deterministically.
#[cfg(unix)]
#[test]
fn published_bytes_are_the_bytes_that_were_hashed() {
    let f = Fixture::new();
    f.task("do nothing").name("root").run().unwrap();
    let fifo = f.dir.join("changing");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let writer = {
        let fifo = fifo.clone();
        std::thread::spawn(move || {
            // The first open sees "first" and then end of file; any second
            // open, once the first has read to the end, sees "second".
            fs::write(&fifo, b"first").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(300));
            let _ = fs::write(&fifo, b"second");
        })
    };
    let published = f
        .yard
        .publish_artifact("root", &fifo, None, None, BTreeMap::new())
        .unwrap();
    // Unblock the writer's second open if publishing never made one: on
    // Linux, opening a FIFO for reading and writing never blocks.
    let unblock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&fifo)
        .unwrap();
    writer.join().unwrap();
    drop(unblock);
    assert_eq!(published.digest, blake3::hash(b"first").to_hex().as_str());
    let out = f.dir.join("out.txt");
    f.yard.read_artifact("root", &published.id, &out).unwrap();
    assert_eq!(fs::read(&out).unwrap(), b"first");
}

/// The same finding with a regular file rewritten in place while it is
/// published: whatever bytes publishing read, the stored blob must hash to
/// the recorded digest, so every read of every publish succeeds.
#[test]
fn a_file_rewritten_while_published_is_stored_consistently() {
    use std::io::{Seek, SeekFrom, Write};
    let f = Fixture::new();
    f.task("do nothing").name("root").run().unwrap();
    let path = f.dir.join("busy.bin");
    let size = 4 * 1024 * 1024;
    fs::write(&path, vec![b'a'; size]).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (path, stop) = (path.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            let mut n = 0u8;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                n = n.wrapping_add(1);
                file.seek(SeekFrom::Start(0)).unwrap();
                file.write_all(&vec![b'a' + n % 26; size]).unwrap();
            }
        })
    };
    let mut published = Vec::new();
    for _ in 0..20 {
        published.push(
            f.yard
                .publish_artifact("root", &path, None, None, BTreeMap::new())
                .unwrap(),
        );
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
    let out = f.dir.join("out.bin");
    for artifact in published {
        f.yard
            .read_artifact("root", &artifact.id, &out)
            .unwrap_or_else(|e| panic!("{} stored inconsistently: {e}", artifact.id));
        assert_eq!(
            blake3::hash(&fs::read(&out).unwrap()).to_hex().as_str(),
            artifact.digest
        );
    }
}
