//! A deterministic USTAR tar writer and reader for portable artifact
//! bundles (`by artifact export`/`import`, [`crate::bundle`]), on the `tar`
//! crate.
//!
//! Every member is a plain file, sorted by the caller before it is passed
//! in, with a fixed mtime, uid, gid, mode, user and group, so the same set
//! of bytes always produces the same archive bytes (`docs/storage.md`
//! "Portable bundles"; `tests/golden/bundle.tar` pins them). [`write`]
//! refuses a name over 100 bytes rather than silently truncating or
//! extending the format with a long-name header.

use std::io::Read;

use crate::Error;

/// Regular file, fixed for every member: `0644`.
const MODE: u32 = 0o644;
/// Fixed at the Unix epoch, never the wall clock, so a re-export of the
/// same artifacts is byte-identical.
const MTIME: u64 = 0;
const UNAME: &str = "branchyard";
const GNAME: &str = "branchyard";

fn corrupt(what: impl std::fmt::Display) -> Error {
    Error::State(format!("tar archive: {what}"))
}

/// One member's USTAR header, checksummed. Refuses a name the format
/// cannot carry rather than corrupting or truncating it.
fn header(name: &str, size: u64) -> Result<tar::Header, Error> {
    if name.len() > 100 {
        return Err(Error::State(format!(
            "tar member name is longer than this bundle format supports: {name}"
        )));
    }
    let mut h = tar::Header::new_ustar();
    h.set_path(name)
        .map_err(|e| Error::State(format!("tar member name {name}: {e}")))?;
    h.set_entry_type(tar::EntryType::Regular);
    h.set_mode(MODE);
    h.set_uid(0);
    h.set_gid(0);
    h.set_size(size);
    h.set_mtime(MTIME);
    h.set_username(UNAME)
        .map_err(|e| corrupt(format_args!("user name: {e}")))?;
    h.set_groupname(GNAME)
        .map_err(|e| corrupt(format_args!("group name: {e}")))?;
    h.set_device_major(0)
        .map_err(|e| corrupt(format_args!("device major: {e}")))?;
    h.set_device_minor(0)
        .map_err(|e| corrupt(format_args!("device minor: {e}")))?;
    // The `tar` crate writes the checksum as seven digits and a space; this
    // format has always written six digits, a NUL and a space (what GNU tar
    // writes too), and bundles are compared byte for byte. Both read back.
    let mut block = *h.as_bytes();
    block[148..156].fill(b' ');
    let sum: u32 = block.iter().map(|b| u32::from(*b)).sum();
    h.as_mut_bytes()[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    Ok(h)
}

/// Write `members` (already in the order they should appear) as a
/// deterministic USTAR tar: fixed mtime/uid/gid/mode, no extended headers,
/// terminated by the two zero blocks the format ends on. The same
/// `members`, in the same order, always produce the same bytes.
pub(crate) fn write(members: &[(String, Vec<u8>)]) -> Result<Vec<u8>, Error> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, bytes) in members {
        let h = header(name, bytes.len() as u64)?;
        builder
            .append(&h, bytes.as_slice())
            .map_err(|e| corrupt(format_args!("write {name}: {e}")))?;
    }
    builder
        .into_inner()
        .map_err(|e| corrupt(format_args!("finish: {e}")))
}

/// Read back what [`write`] wrote: every member's name and bytes, in
/// archive order. Refuses a truncated archive, a member whose header
/// checksum does not match its own bytes (corruption, not the caller's
/// content digest, which [`crate::bundle`] checks separately), and any
/// entry type other than a regular file, since this format never writes
/// one.
pub(crate) fn read(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let mut archive = tar::Archive::new(bytes);
    let entries = archive
        .entries()
        .map_err(|e| corrupt(format_args!("unreadable: {e}")))?;
    let mut members = Vec::new();
    for entry in entries {
        let mut entry = entry.map_err(|e| corrupt(format_args!("{e}")))?;
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let kind = entry.header().entry_type();
        if kind != tar::EntryType::Regular {
            return Err(corrupt(format_args!(
                "member {name} is not a regular file (typeflag {}); this bundle format never \
                 writes one",
                kind.as_byte()
            )));
        }
        let mut data = Vec::new();
        entry
            .read_to_end(&mut data)
            .map_err(|e| corrupt(format_args!("truncated: member {name}: {e}")))?;
        members.push((name, data));
    }
    Ok(members)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_round_trip_returns_the_same_members_in_order() {
        let members = vec![
            ("index.json".to_owned(), b"{}".to_vec()),
            ("artifacts/1".to_owned(), vec![7u8; 1000]),
            ("artifacts/2".to_owned(), Vec::new()),
        ];
        let archive = write(&members).unwrap();
        assert_eq!(read(&archive).unwrap(), members);
    }

    #[test]
    fn the_same_members_always_produce_the_same_bytes() {
        let members = vec![("a".to_owned(), vec![1, 2, 3])];
        assert_eq!(write(&members).unwrap(), write(&members).unwrap());
    }

    #[test]
    fn a_name_over_100_bytes_is_refused() {
        let name = "x".repeat(101);
        assert!(write(&[(name, vec![1])]).is_err());
    }

    #[test]
    fn a_truncated_archive_is_refused() {
        let members = vec![("a".to_owned(), vec![1, 2, 3])];
        let mut archive = write(&members).unwrap();
        archive.truncate(archive.len() - 600);
        assert!(read(&archive).is_err());
    }

    #[test]
    fn a_flipped_header_byte_fails_the_checksum() {
        let members = vec![("a".to_owned(), vec![1, 2, 3])];
        let mut archive = write(&members).unwrap();
        archive[5] ^= 0xFF; // inside the name field
        assert!(read(&archive).is_err());
    }

    #[test]
    fn tampered_content_still_reads_back_since_the_header_checksum_covers_only_the_header() {
        // The tar-level checksum protects the header, not the content;
        // `crate::bundle` checks content against the blake3 digest in its
        // index. This documents that division of labor rather than
        // asserting a guarantee this module does not make.
        let members = vec![("a".to_owned(), vec![1, 2, 3])];
        let mut archive = write(&members).unwrap();
        let data_start = 512;
        archive[data_start] ^= 0xFF;
        let back = read(&archive).unwrap();
        assert_eq!(back[0].1, vec![254, 2, 3]);
    }

    /// The members behind `tests/golden/bundle.tar`: an index and three
    /// artifacts of 0, 512 (a whole block, so no padding) and 1337 bytes.
    fn golden_members() -> Vec<(String, Vec<u8>)> {
        let pattern = |len: usize| -> Vec<u8> { (0..len).map(|i| (i * 7 % 251) as u8).collect() };
        vec![
            (
                "index.json".to_owned(),
                b"{\n  \"version\": 1,\n  \"entries\": []\n}".to_vec(),
            ),
            ("artifacts/art_0001".to_owned(), pattern(0)),
            ("artifacts/art_0002".to_owned(), pattern(512)),
            ("artifacts/art_0003".to_owned(), pattern(1337)),
        ]
    }

    /// The bytes the hand-written writer produced before the `tar` crate
    /// took over, for these members: a bundle exported by an older build
    /// must stay byte-identical (and so must its digest).
    #[test]
    fn the_archive_bytes_match_the_golden_file() {
        let golden = include_bytes!("../tests/golden/bundle.tar");
        let written = write(&golden_members()).unwrap();
        assert!(written == golden, "the bundle tar bytes changed");
        assert_eq!(read(golden).unwrap(), golden_members());
    }
}
