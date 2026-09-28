//! A minimal, deterministic USTAR tar writer and reader for portable
//! artifact bundles (`by artifact export`/`import`, [`crate::bundle`]).
//!
//! Not a general tar implementation: every member is a plain file, sorted
//! by the caller before it is passed in, with a fixed mtime, uid, gid and
//! mode, so the same set of bytes always produces the same archive bytes
//! (`docs/storage.md` "Portable bundles"). No long-name (GNU) extension,
//! no directories, no symlinks: [`write`] refuses a name over 100 bytes
//! rather than silently truncating or extending the format.

use crate::Error;

const BLOCK: usize = 512;
/// Regular file, fixed for every member: `0644`.
const MODE: u32 = 0o644;
/// Fixed at the Unix epoch, never the wall clock, so a re-export of the
/// same artifacts is byte-identical.
const MTIME: u64 = 0;
const UNAME: &str = "branchyard";
const GNAME: &str = "branchyard";

fn octal_field(value: u64, width: usize) -> Vec<u8> {
    // `width` includes the trailing NUL.
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

fn set(header: &mut [u8; BLOCK], offset: usize, field: &[u8]) {
    header[offset..offset + field.len()].copy_from_slice(field);
}

/// One member's USTAR header, checksummed. Refuses a name the format
/// cannot carry rather than corrupting or truncating it.
fn header(name: &str, size: u64) -> Result<[u8; BLOCK], Error> {
    if name.len() > 100 {
        return Err(Error::State(format!(
            "tar member name is longer than this bundle format supports: {name}"
        )));
    }
    let mut h = [0u8; BLOCK];
    set(&mut h, 0, &str_field(name, 100));
    set(&mut h, 100, &octal_field(MODE as u64, 8));
    set(&mut h, 108, &octal_field(0, 8)); // uid
    set(&mut h, 116, &octal_field(0, 8)); // gid
    set(&mut h, 124, &octal_field(size, 12));
    set(&mut h, 136, &octal_field(MTIME, 12));
    set(&mut h, 148, b"        "); // chksum: spaces while computing
    h[156] = b'0'; // typeflag: regular file
    set(&mut h, 257, b"ustar\0");
    set(&mut h, 263, b"00");
    set(&mut h, 265, &str_field(UNAME, 32));
    set(&mut h, 297, &str_field(GNAME, 32));
    set(&mut h, 329, &octal_field(0, 8)); // devmajor
    set(&mut h, 337, &octal_field(0, 8)); // devminor
    let checksum: u32 = h.iter().map(|b| u32::from(*b)).sum();
    let field = format!("{checksum:06o}\0 ");
    set(&mut h, 148, field.as_bytes());
    Ok(h)
}

fn pad_len(size: u64) -> usize {
    let rem = (size as usize) % BLOCK;
    if rem == 0 {
        0
    } else {
        BLOCK - rem
    }
}

/// Write `members` (already in the order they should appear) as a
/// deterministic USTAR tar: fixed mtime/uid/gid/mode, no extended headers,
/// terminated by the two zero blocks the format ends on. The same
/// `members`, in the same order, always produce the same bytes.
pub(crate) fn write(members: &[(String, Vec<u8>)]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    for (name, bytes) in members {
        out.extend_from_slice(&header(name, bytes.len() as u64)?);
        out.extend_from_slice(bytes);
        out.resize(out.len() + pad_len(bytes.len() as u64), 0);
    }
    out.resize(out.len() + 2 * BLOCK, 0);
    Ok(out)
}

fn parse_octal(field: &[u8]) -> Result<u64, Error> {
    let text = String::from_utf8_lossy(field);
    let text = text.trim_matches(|c: char| c == '\0' || c == ' ');
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text, 8).map_err(|_| {
        Error::State(format!(
            "tar header has a malformed numeric field: {text:?}"
        ))
    })
}

fn parse_name(field: &[u8]) -> String {
    let end = field.iter().position(|b| *b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// Read back what [`write`] wrote: every member's name and bytes, in
/// archive order. Refuses a truncated archive, a member whose header
/// checksum does not match its own bytes (corruption, not the caller's
/// content digest, which [`crate::bundle`] checks separately), and any
/// entry type other than a regular file, since this format never writes
/// one.
pub(crate) fn read(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let mut members = Vec::new();
    let mut offset = 0usize;
    loop {
        if offset + BLOCK > bytes.len() {
            return Err(Error::State(
                "tar archive is truncated: an incomplete header".into(),
            ));
        }
        let header = &bytes[offset..offset + BLOCK];
        if header.iter().all(|b| *b == 0) {
            break; // the end-of-archive marker; the rest is padding.
        }
        let recorded: u32 = parse_octal(&header[148..156])? as u32;
        let mut for_sum = header.to_vec();
        for_sum[148..156].copy_from_slice(b"        ");
        let computed: u32 = for_sum.iter().map(|b| u32::from(*b)).sum();
        if computed != recorded {
            return Err(Error::State(format!(
                "tar header checksum mismatch at offset {offset}: the archive is corrupted"
            )));
        }
        let typeflag = header[156];
        if typeflag != b'0' && typeflag != 0 {
            return Err(Error::State(format!(
                "tar member is not a regular file (typeflag {typeflag}); this bundle format never writes one"
            )));
        }
        let name = parse_name(&header[0..100]);
        let size = parse_octal(&header[124..136])?;
        let data_start = offset + BLOCK;
        let data_end = data_start
            .checked_add(size as usize)
            .ok_or_else(|| Error::State("tar member size overflows".into()))?;
        if data_end > bytes.len() {
            return Err(Error::State(format!(
                "tar archive is truncated: member {name} claims {size} bytes past the end"
            )));
        }
        members.push((name, bytes[data_start..data_end].to_vec()));
        offset = data_end + pad_len(size);
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
        let data_start = BLOCK;
        archive[data_start] ^= 0xFF;
        let back = read(&archive).unwrap();
        assert_eq!(back[0].1, vec![254, 2, 3]);
    }
}
