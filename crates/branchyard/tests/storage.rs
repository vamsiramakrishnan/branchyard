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
