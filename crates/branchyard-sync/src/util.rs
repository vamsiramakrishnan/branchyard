//! Small helpers every layer shares: base64, dates in the formats the
//! cloud APIs sign and return, percent-encoding, and randomness.

use base64::Engine as _;
use branchyard_client::http::decode_form;
use ring::rand::{SecureRandom, SystemRandom};

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

/// `n` bytes from the system's secure generator.
pub fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut out = vec![0u8; n];
    SystemRandom::new()
        .fill(&mut out)
        .map_err(|_| Error::local("the system random generator failed"))?;
    Ok(out)
}

/// A random 64-bit seed, for jitter.
pub fn random_seed() -> u64 {
    random_bytes(8)
        .map(|b| u64::from_le_bytes(b.try_into().unwrap_or([7; 8])))
        .unwrap_or(0x9e37_79b9_7f4a_7c15)
}

/// SplitMix64: a small, seedable generator for jitter and sampling. Not
/// for keys.
#[derive(Clone, Debug)]
pub struct SplitMix(u64);

impl SplitMix {
    pub fn new(seed: u64) -> SplitMix {
        SplitMix(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..=max`.
    pub fn below_or_at(&mut self, max: u64) -> u64 {
        if max == u64::MAX {
            return self.next_u64();
        }
        self.next_u64() % (max + 1)
    }
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

struct Parts {
    y: i64,
    mo: u32,
    d: u32,
    h: u64,
    mi: u64,
    s: u64,
    weekday: usize,
}

fn parts(ms: u64) -> Parts {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, mo, d) = civil(days);
    Parts {
        y,
        mo,
        d,
        h: rem / 3600,
        mi: rem % 3600 / 60,
        s: rem % 60,
        // 1970-01-01 was a Thursday.
        weekday: ((days + 4).rem_euclid(7)) as usize,
    }
}

/// `20150830T123600Z`, as SigV4 signs.
pub fn amz_date(ms: u64) -> String {
    let p = parts(ms);
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        p.y, p.mo, p.d, p.h, p.mi, p.s
    )
}

/// `Sun, 30 Aug 2015 12:36:00 GMT`, as Azure's `x-ms-date` takes.
pub fn http_date(ms: u64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let p = parts(ms);
    format!(
        "{}, {:02} {} {:04} {:02}:{:02}:{:02} GMT",
        DAYS[p.weekday],
        p.d,
        MONTHS[(p.mo - 1) as usize],
        p.y,
        p.h,
        p.mi,
        p.s
    )
}

/// `2015-08-30T12:36:00.000Z`.
pub fn rfc3339(ms: u64) -> String {
    let p = parts(ms);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        p.y,
        p.mo,
        p.d,
        p.h,
        p.mi,
        p.s,
        ms % 1000
    )
}

/// Milliseconds from `2015-08-30T12:36:00Z` or with fractional seconds or
/// an offset of `Z`; `None` for anything else.
pub fn parse_rfc3339(text: &str) -> Option<u64> {
    let text = text.trim();
    let (date, time) = text.split_once('T')?;
    let mut d = date.split('-');
    let (y, mo, da): (i64, u32, u32) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let time = time.strip_suffix('Z')?;
    let (hms, frac) = match time.split_once('.') {
        Some((hms, frac)) => (hms, frac),
        None => (time, "0"),
    };
    let mut t = hms.split(':');
    let (h, mi, s): (u64, u64, u64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    let mut frac_ms = frac.chars().take(3).collect::<String>();
    while frac_ms.len() < 3 {
        frac_ms.push('0');
    }
    let days = days_from_civil(y, mo, da);
    if days < 0 {
        return None;
    }
    Some((days as u64 * 86_400 + h * 3600 + mi * 60 + s) * 1000 + frac_ms.parse::<u64>().ok()?)
}

/// Milliseconds from an RFC 1123 date, `Sun, 30 Aug 2015 12:36:00 GMT`.
pub fn parse_http_date(text: &str) -> Option<u64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut words = text.split_whitespace().skip(1);
    let d: u32 = words.next()?.parse().ok()?;
    let month = words.next()?;
    let mo = MONTHS.iter().position(|m| *m == month)? as u32 + 1;
    let y: i64 = words.next()?.parse().ok()?;
    let mut t = words.next()?.split(':');
    let (h, mi, s): (u64, u64, u64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    let days = days_from_civil(y, mo, d);
    if days < 0 {
        return None;
    }
    Some((days as u64 * 86_400 + h * 3600 + mi * 60 + s) * 1000)
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
    fn dates_format_and_parse() {
        // 2015-08-30T12:36:00Z, the AWS test suite's date.
        let ms = 1_440_938_160_000;
        assert_eq!(amz_date(ms), "20150830T123600Z");
        assert_eq!(http_date(ms), "Sun, 30 Aug 2015 12:36:00 GMT");
        assert_eq!(rfc3339(ms + 5), "2015-08-30T12:36:00.005Z");
        assert_eq!(parse_rfc3339("2015-08-30T12:36:00.005Z"), Some(ms + 5));
        assert_eq!(parse_rfc3339("2015-08-30T12:36:00Z"), Some(ms));
        assert_eq!(parse_http_date("Sun, 30 Aug 2015 12:36:00 GMT"), Some(ms));
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
