//! Portable artifact bundles (`Yard::export_artifacts`/`import_artifacts`,
//! `by artifact export`/`import`): round trip, byte-identical re-export, a
//! tampered, missing or extra member each refusing the whole import, and
//! read authorization on export. See `docs/storage.md` "Portable bundles".

mod common;

use std::collections::BTreeMap;
use std::fs;

use branchyard::Error;
use common::Fixture;

/// A small USTAR reader/writer, independent of `branchyard`'s own
/// (private) `tarball` module, used here only to tamper with an otherwise
/// valid bundle for the refusal tests below. Must produce archives
/// `branchyard::Yard::import_artifacts` can still parse, so it follows the
/// same header layout.
mod tar_test_support {
    const BLOCK: usize = 512;

    fn octal_field(value: u64, width: usize) -> Vec<u8> {
        let digits = width - 1;
        let mut out = format!("{value:0digits$o}").into_bytes();
        out.push(0);
        out
    }

    fn str_field(value: &str, width: usize) -> Vec<u8> {
        let mut out = vec![0u8; width];
        let bytes = value.as_bytes();
        let n = bytes.len().min(width);
        out[..n].copy_from_slice(&bytes[..n]);
        out
    }

    fn header(name: &str, size: u64) -> [u8; BLOCK] {
        let mut h = [0u8; BLOCK];
        h[0..100].copy_from_slice(&str_field(name, 100));
        h[100..108].copy_from_slice(&octal_field(0o644, 8));
        h[108..116].copy_from_slice(&octal_field(0, 8));
        h[116..124].copy_from_slice(&octal_field(0, 8));
        h[124..136].copy_from_slice(&octal_field(size, 12));
        h[136..148].copy_from_slice(&octal_field(0, 12));
        h[148..156].copy_from_slice(b"        ");
        h[156] = b'0';
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[265..265 + 10].copy_from_slice(b"branchyard");
        h[297..297 + 10].copy_from_slice(b"branchyard");
        h[329..337].copy_from_slice(&octal_field(0, 8));
        h[337..345].copy_from_slice(&octal_field(0, 8));
        let checksum: u32 = h.iter().map(|b| u32::from(*b)).sum();
        let field = format!("{checksum:06o}\0 ");
        h[148..148 + field.len()].copy_from_slice(field.as_bytes());
        h
    }

    fn pad_len(size: u64) -> usize {
        let rem = (size as usize) % BLOCK;
        if rem == 0 {
            0
        } else {
            BLOCK - rem
        }
    }

    pub fn write_tar(members: &[(String, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, bytes) in members {
            out.extend_from_slice(&header(name, bytes.len() as u64));
            out.extend_from_slice(bytes);
            out.resize(out.len() + pad_len(bytes.len() as u64), 0);
        }
        out.resize(out.len() + 2 * BLOCK, 0);
        out
    }

    fn parse_octal(field: &[u8]) -> u64 {
        let text = String::from_utf8_lossy(field);
        let text = text.trim_matches(|c: char| c == '\0' || c == ' ');
        if text.is_empty() {
            0
        } else {
            u64::from_str_radix(text, 8).unwrap()
        }
    }

    fn parse_name(field: &[u8]) -> String {
        let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
        String::from_utf8_lossy(&field[..end]).into_owned()
    }

    pub fn read_tar(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut members = Vec::new();
        let mut offset = 0usize;
        loop {
            if offset + BLOCK > bytes.len() {
                break;
            }
            let header = &bytes[offset..offset + BLOCK];
            if header.iter().all(|b| *b == 0) {
                break;
            }
            let name = parse_name(&header[0..100]);
            let size = parse_octal(&header[124..136]);
            let data_start = offset + BLOCK;
            let data_end = data_start + size as usize;
            members.push((name, bytes[data_start..data_end].to_vec()));
            offset = data_end + pad_len(size);
        }
        members
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
