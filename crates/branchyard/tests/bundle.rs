//! Portable artifact bundles (`Yard::export_artifacts`/`import_artifacts`,
//! `by artifact export`/`import`): round trip, byte-identical re-export, a
//! tampered, missing or extra member each refusing the whole import, and
//! read authorization on export. See `docs/storage.md` "Portable bundles".

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
mod common;

use std::collections::BTreeMap;
use std::fs;

use branchyard::Error;
use common::Fixture;

/// Read and write a bundle's tar with the `tar` crate, independent of
/// `branchyard`'s own (private) `tarball` module, used here only to tamper
/// with an otherwise valid bundle for the refusal tests below.
mod tar_test_support {
    use std::io::Read;

    pub fn write_tar(members: &[(String, Vec<u8>)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, bytes) in members {
            let mut header = tar::Header::new_ustar();
            header.set_path(name).unwrap();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, bytes.as_slice()).unwrap();
        }
        builder.into_inner().unwrap()
    }

    pub fn read_tar(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        tar::Archive::new(bytes)
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let name = entry.path().unwrap().to_string_lossy().into_owned();
                let mut data = Vec::new();
                entry.read_to_end(&mut data).unwrap();
                (name, data)
            })
            .collect()
    }
}

fn publish(f: &Fixture, branch: &str, name: &str, content: &[u8]) -> branchyard::ArtifactRef {
    let path = f.dir.join(name);
    fs::write(&path, content).unwrap();
    f.yard
        .publish_artifact(branch, &path, Some(name.to_owned()), None, BTreeMap::new())
        .unwrap()
}

#[test]
fn a_round_trip_imports_the_same_bytes_with_original_provenance_recorded() {
    let f = Fixture::new();
    f.task("do nothing").name("root").run().unwrap();
    f.task("do nothing").name("other").run().unwrap();

    let a1 = publish(&f, "root", "one.txt", b"hello");
    let a2 = publish(&f, "root", "two.txt", b"a bit more content here");
    let bundle = f.dir.join("bundle.tar");
    let entries = f
        .yard
        .export_artifacts("root", &[a1.id.clone(), a2.id.clone()], &bundle)
        .unwrap();
    assert_eq!(entries.len(), 2);
    assert!(bundle.is_file());

    // Owned by a different branch than the one that published them.
    let imported = f.yard.import_artifacts("other", &bundle).unwrap();
    assert_eq!(imported.len(), 2);
    assert!(imported.iter().all(|a| a.publisher_branch == "other"));
    assert!(imported.iter().all(|a| a.id != a1.id && a.id != a2.id));

    let mut bytes = Vec::new();
    for a in &imported {
        let out = f.dir.join(format!("out-{}", a.id));
        let read = f.yard.read_artifact("other", &a.id, &out).unwrap();
        assert_eq!(read, *a);
        bytes.push(fs::read(&out).unwrap());
        assert_eq!(
            a.labels.get("bundle.origin_publisher").map(String::as_str),
            Some("root")
        );
        assert!(a.labels.contains_key("bundle.origin_id"));
        assert!(a.labels.contains_key("bundle.origin_created_at"));
    }
    assert!(bytes.contains(&b"hello".to_vec()));
    assert!(bytes.contains(&b"a bit more content here".to_vec()));

    // The originals are untouched and still readable under their own ids.
    assert_eq!(
        f.yard
            .read_artifact("root", &a1.id, f.dir.join("orig1"))
            .unwrap()
            .digest,
        entries.iter().find(|e| e.id == a1.id).unwrap().digest
    );
}

#[test]
fn re_exporting_the_same_artifacts_is_byte_identical() {
    let f = Fixture::new();
    f.task("do nothing").name("root").run().unwrap();
    let a1 = publish(&f, "root", "one.txt", b"hello, deterministically");

    let bundle1 = f.dir.join("b1.tar");
    let bundle2 = f.dir.join("b2.tar");
    f.yard
        .export_artifacts("root", std::slice::from_ref(&a1.id), &bundle1)
        .unwrap();
    f.yard
        .export_artifacts("root", std::slice::from_ref(&a1.id), &bundle2)
        .unwrap();
    assert_eq!(fs::read(&bundle1).unwrap(), fs::read(&bundle2).unwrap());
}

#[test]
fn a_tampered_member_is_refused_and_nothing_is_imported() {
    let f = Fixture::new();
    f.task("do nothing").name("root").run().unwrap();
    f.task("do nothing").name("other").run().unwrap();
    let a1 = publish(&f, "root", "one.txt", b"hello");
    let bundle = f.dir.join("bundle.tar");
    f.yard
        .export_artifacts("root", std::slice::from_ref(&a1.id), &bundle)
        .unwrap();

    // Flip a byte in the artifact member's content (not its header), so
    // the tar itself still parses and only the content digest disagrees
    // with the index.
    let mut members = tar_test_support::read_tar(&fs::read(&bundle).unwrap());
    let member = members
        .iter_mut()
        .find(|(name, _)| name.starts_with("artifacts/"))
        .unwrap();
    member.1[0] ^= 0xFF;
    fs::write(&bundle, tar_test_support::write_tar(&members)).unwrap();

    let err = f.yard.import_artifacts("other", &bundle).unwrap_err();
    assert!(
        matches!(&err, Error::State(msg) if msg.contains("digest")),
        "{err}"
    );
    assert!(
        f.yard.artifacts("other").unwrap().is_empty(),
        "a refused import must publish nothing"
    );
}

#[test]
fn a_missing_indexed_member_is_refused() {
    let f = Fixture::new();
    f.task("do nothing").name("root").run().unwrap();
    f.task("do nothing").name("other").run().unwrap();
    let a1 = publish(&f, "root", "one.txt", b"hello");
    let a2 = publish(&f, "root", "two.txt", b"world");
    let bundle = f.dir.join("bundle.tar");
    f.yard
        .export_artifacts("root", &[a1.id.clone(), a2.id.clone()], &bundle)
        .unwrap();

    // Drop one artifact's tar member but keep the index naming both.
    let members = tar_test_support::read_tar(&fs::read(&bundle).unwrap());
    let kept: Vec<_> = members
        .into_iter()
        .filter(|(name, _)| !name.ends_with(&a2.id))
        .collect();
    fs::write(&bundle, tar_test_support::write_tar(&kept)).unwrap();

    let err = f.yard.import_artifacts("other", &bundle).unwrap_err();
    assert!(
        matches!(&err, Error::State(msg) if msg.contains("no member")),
        "{err}"
    );
    assert!(f.yard.artifacts("other").unwrap().is_empty());
}

#[test]
fn an_extra_unindexed_member_is_refused() {
    let f = Fixture::new();
    f.task("do nothing").name("root").run().unwrap();
    f.task("do nothing").name("other").run().unwrap();
    let a1 = publish(&f, "root", "one.txt", b"hello");
    let bundle = f.dir.join("bundle.tar");
    f.yard
        .export_artifacts("root", std::slice::from_ref(&a1.id), &bundle)
        .unwrap();

    let mut members = tar_test_support::read_tar(&fs::read(&bundle).unwrap());
    members.push(("artifacts/not-in-the-index".to_owned(), b"sneaky".to_vec()));
    fs::write(&bundle, tar_test_support::write_tar(&members)).unwrap();

    let err = f.yard.import_artifacts("other", &bundle).unwrap_err();
    assert!(
        matches!(&err, Error::State(msg) if msg.contains("extra member")),
        "{err}"
    );
    assert!(f.yard.artifacts("other").unwrap().is_empty());
}

#[test]
fn export_refuses_without_read_access_and_needs_at_least_one_id() {
    let f = Fixture::new();
    f.task("do nothing").name("root").run().unwrap();
    f.task("do nothing").name("sibling").run().unwrap();
    let a1 = publish(&f, "root", "one.txt", b"hello");

    assert!(f
        .yard
        .export_artifacts("sibling", std::slice::from_ref(&a1.id), f.dir.join("x.tar"))
        .is_err());
    assert!(f
        .yard
        .export_artifacts("root", &[], f.dir.join("empty.tar"))
        .is_err());
}
