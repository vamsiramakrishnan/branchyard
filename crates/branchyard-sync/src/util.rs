//! Small helpers every layer shares: base64, percent-encoding and random
//! bytes. Hex is the `hex` crate. Dates in the formats the cloud APIs sign
//! and return are `branchyard_support::time`; the jitter generator is
//! `branchyard_support::rng`.

use base64::Engine as _;
use branchyard_client::http::decode_form;

use crate::error::{Error, Result};

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
    let encoded = branchyard_client::http::encode(text);
    match keep_slash {
        true => encoded.replace("%2F", "/"),
        false => encoded,
    }
}

/// Parse `a=b&c=d` into decoded pairs.
pub fn query_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (decode_form(k), decode_form(v)),
            None => (decode_form(p), String::new()),
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
        assert_eq!(uri_encode("100%", true), "100%25");
        assert_eq!(
            hex::decode(hex::encode([0, 1, 254, 255])).unwrap(),
            vec![0, 1, 254, 255]
        );
        assert!(hex::decode("abc").is_err());
        assert_eq!(unb64url(&b64url(b"hello?")).unwrap(), b"hello?");
        let pairs = query_pairs("endpoint=http%3A%2F%2Fx&flag");
        assert_eq!(pairs[0], ("endpoint".into(), "http://x".into()));
        assert_eq!(pairs[1], ("flag".into(), String::new()));
        // Form values: `+` is a space, `%2B` a plus, a trailing escape decodes.
        let pairs = query_pairs("a=b+c%2Bd&e=%41");
        assert_eq!(pairs[0], ("a".into(), "b c+d".into()));
        assert_eq!(pairs[1], ("e".into(), "A".into()));
    }
}
