//! Small helpers every layer shares: hex, base64, percent-encoding and
//! random bytes. Dates in the formats the cloud APIs sign and return are
//! `branchyard_support::time`; the jitter generator is
//! `branchyard_support::rng`.

use base64::Engine as _;

use crate::error::{Error, Result};

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 15) as usize] as char);
    }
    out
}

pub fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn unb64(text: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .map_err(|e| Error::corrupt(format!("bad base64: {e}")))
}

pub fn b64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn unb64url(text: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.trim().trim_end_matches('='))
        .map_err(|e| Error::corrupt(format!("bad base64url: {e}")))
}

/// `n` bytes from the system's secure generator; its failure is the caller's.
pub fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; n];
    branchyard_support::rng::fill_random(&mut out).map_err(|e| Error::local(e.to_string()))?;
    Ok(out)
}

/// Percent-encode as SigV4 and the cloud APIs want: everything but
/// `A-Z a-z 0-9 - . _ ~`, and `/` too unless `keep_slash`.
pub fn uri_encode(text: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Undo percent-encoding (and `+` as a space when `plus`).
pub fn uri_decode(text: &str, plus: bool) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .ok_or(())
                {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' if plus => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse `a=b&c=d` into decoded pairs.
pub fn query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (uri_decode(k, true), uri_decode(v, true)),
            None => (uri_decode(p, true), String::new()),
        })
        .collect()
}

/// Escape text for an XML body.
pub fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cloud_date_formats_are_the_apis_examples() {
        use branchyard_support::time::{
            amz_date, http_date, parse_http_date, parse_rfc3339, rfc3339,
        };
        // 2015-08-30T12:36:00Z, the AWS test suite's date.
        let ms = 1_440_938_160_000;
        assert_eq!(amz_date(ms), "20150830T123600Z");
        assert_eq!(http_date(ms), "Sun, 30 Aug 2015 12:36:00 GMT");
        assert_eq!(rfc3339(ms + 5), "2015-08-30T12:36:00.005Z");
        assert_eq!(parse_rfc3339("2015-08-30T12:36:00.005Z"), Ok(ms + 5));
        assert_eq!(parse_rfc3339("2015-08-30T12:36:00Z"), Ok(ms));
        assert_eq!(parse_http_date("Sun, 30 Aug 2015 12:36:00 GMT"), Ok(ms));
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(http_date(951_782_400_000), "Tue, 29 Feb 2000 00:00:00 GMT");
    }

    #[test]
    fn encodings_round_trip() {
        assert_eq!(uri_encode("a b/c~d", true), "a%20b/c~d");
        assert_eq!(uri_encode("a b/c", false), "a%20b%2Fc");
        assert_eq!(uri_decode("a%20b%2Fc+d", true), "a b/c d");
        assert_eq!(uri_decode("100%", false), "100%");
        assert_eq!(
            unhex(&hex(&[0, 1, 254, 255])).unwrap(),
            vec![0, 1, 254, 255]
        );
        assert!(unhex("abc").is_none());
        assert_eq!(unb64url(&b64url(b"hello?")).unwrap(), b"hello?");
        let pairs = query_pairs("endpoint=http%3A%2F%2Fx&flag");
        assert_eq!(pairs[0], ("endpoint".into(), "http://x".into()));
        assert_eq!(pairs[1], ("flag".into(), String::new()));
    }
}
