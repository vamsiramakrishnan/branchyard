//! The one clock: the time now, and every date format Branchyard reads or
//! writes, on `jiff`.
//!
//! Times are milliseconds since the Unix epoch in a `u64`, the shape every
//! record, event and wire format here already uses. This module is the only
//! place that reads the system clock and the only place that turns those
//! milliseconds into text or back, so there is one answer to "what does the
//! clock say" and one answer to "how is it spelled":
//!
//! - [`now_ms`] and [`now_nanos`] read the clock. A clock set before 1970
//!   reads as zero and says so once, instead of panicking or hiding it.
//! - [`rfc3339`] (`2026-09-26T12:34:56.789Z`), [`rfc3339_secs`]
//!   (`2026-10-03T12:00:00Z`), [`utc_minute`] (`2026-10-01 14:30 UTC`),
//!   [`amz_date`] (`20150830T123600Z`) and [`http_date`]
//!   (`Sun, 30 Aug 2015 12:36:00 GMT`) write them.
//! - [`parse_rfc3339`] and [`parse_http_date`] read them back and refuse
//!   anything malformed with a [`TimeError`] that says why.
//! - [`period_starts`] is the start of the UTC day and month.
//! - [`parse_duration`] and [`human_duration`] read and write `30s`, `5m`.
//!
//! Formatting never fails: a time past the end of year 9999 (the end of the
//! calendar `jiff` can name) is written as 9999-12-31T23:59:59.999Z. See CONTRIBUTING.md
//! ("Time, ids and randomness").

use std::fmt;
use std::sync::Once;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jiff::civil::DateTime;
use jiff::fmt::temporal::Pieces;
use jiff::SignedDuration;

/// A time or duration that could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeError(String);

impl TimeError {
    fn new(message: impl Into<String>) -> TimeError {
        TimeError(message.into())
    }
}

impl fmt::Display for TimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TimeError {}

/// The system clock since the epoch; a clock before 1970 is reported once
/// and read as zero.
fn since_epoch() -> Duration {
    static BEFORE_EPOCH: Once = Once::new();
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed,
        Err(_) => {
            BEFORE_EPOCH.call_once(|| {
                tracing::warn!(
                    "the system clock is set before 1970-01-01; reading it as the epoch"
                );
            });
            Duration::ZERO
        }
    }
}

/// Milliseconds since the Unix epoch, now.
pub fn now_ms() -> u64 {
    u64::try_from(since_epoch().as_millis()).unwrap_or(u64::MAX)
}

/// Nanoseconds since the Unix epoch, now; for names that must differ from
/// one call to the next, not for ordering.
pub fn now_nanos() -> u128 {
    since_epoch().as_nanos()
}

/// Milliseconds since the epoch of a file time or other [`SystemTime`];
/// `None` before 1970.
pub fn system_time_ms(time: SystemTime) -> Option<u64> {
    let elapsed = time.duration_since(UNIX_EPOCH).ok()?;
    Some(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

/// Nanoseconds since the epoch of a file time or other [`SystemTime`];
/// `None` before 1970. Finer than [`system_time_ms`], for change detection.
pub fn system_time_nanos(time: SystemTime) -> Option<u128> {
    Some(time.duration_since(UNIX_EPOCH).ok()?.as_nanos())
}

/// The last millisecond `jiff`'s civil calendar names: 9999-12-31T23:59:59.999Z.
const MAX_MS: u64 = 253_402_300_799_999;

fn epoch() -> DateTime {
    jiff::civil::date(1970, 1, 1).at(0, 0, 0, 0)
}

/// The UTC civil time `ms` milliseconds after the epoch; the last one the
/// calendar can name when `ms` is past it. (Not a `jiff::Timestamp`, which
/// stops a day short of the calendar, at 9999-12-30T22:00Z.)
fn civil_at(ms: u64) -> DateTime {
    let ms = i64::try_from(ms.min(MAX_MS)).unwrap_or(i64::MAX);
    epoch()
        .checked_add(SignedDuration::from_millis(ms))
        .unwrap_or(DateTime::MAX)
}

/// Milliseconds since the epoch of the UTC civil time `at`; `None` before it.
fn ms_of(at: DateTime) -> Option<u64> {
    u64::try_from(at.duration_since(epoch()).as_millis()).ok()
}

/// `2026-09-26T12:34:56.789Z`: UTC, always three fractional digits.
pub fn rfc3339(ms: u64) -> String {
    civil_at(ms).strftime("%Y-%m-%dT%H:%M:%S.%3fZ").to_string()
}

/// `2026-10-03T12:00:00Z`: UTC to the second.
pub fn rfc3339_secs(ms: u64) -> String {
    civil_at(ms).strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `2026-10-01 14:30 UTC`: UTC to the minute, for people.
pub fn utc_minute(ms: u64) -> String {
    civil_at(ms).strftime("%Y-%m-%d %H:%M UTC").to_string()
}

/// `20150830T123600Z`, as AWS Signature V4 signs.
pub fn amz_date(ms: u64) -> String {
    civil_at(ms).strftime("%Y%m%dT%H%M%SZ").to_string()
}

/// `Sun, 30 Aug 2015 12:36:00 GMT`, as HTTP and Azure's `x-ms-date` take.
pub fn http_date(ms: u64) -> String {
    civil_at(ms)
        .strftime("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

/// Milliseconds since the epoch of an RFC 3339 time: `Z` or a `+hh:mm`
/// offset, whole or fractional seconds (cut to milliseconds).
///
/// Anything else is an error: a missing offset, a month 13, an hour 24, a
/// bracketed time zone, text after the time, or a time before 1970.
pub fn parse_rfc3339(text: &str) -> Result<u64, TimeError> {
    let text = text.trim();
    let invalid = |why: &str| TimeError::new(format!("{text:?} is not an RFC 3339 time ({why})"));
    if text.len() < 20 || !matches!(text.as_bytes().get(10), Some(b'T' | b't')) {
        return Err(invalid("expected 2026-09-30T12:34:56Z"));
    }
    if text.contains('[') {
        return Err(invalid("it names a time zone; only an offset is allowed"));
    }
    let pieces = Pieces::parse(text).map_err(|e| invalid(&e.to_string()))?;
    let time = pieces.time().ok_or_else(|| invalid("it has no time"))?;
    let offset = pieces
        .to_numeric_offset()
        .ok_or_else(|| invalid("it has no Z or offset"))?;
    if offset.seconds().abs() >= 86_400 {
        return Err(invalid("the offset is a day or more"));
    }
    let local = pieces.date().to_datetime(time);
    let utc = local
        .checked_sub(SignedDuration::from_secs(i64::from(offset.seconds())))
        .map_err(|e| invalid(&e.to_string()))?;
    ms_of(utc).ok_or_else(|| TimeError::new(format!("{text:?} is before 1970-01-01")))
}

/// Milliseconds since the epoch of an RFC 1123 date,
/// `Sun, 30 Aug 2015 12:36:00 GMT`.
pub fn parse_http_date(text: &str) -> Result<u64, TimeError> {
    let text = text.trim();
    let invalid = || {
        TimeError::new(format!(
            "{text:?} is not an HTTP date such as Sun, 30 Aug 2015 12:36:00 GMT"
        ))
    };
    let body = text.strip_suffix("GMT").ok_or_else(invalid)?.trim_end();
    let civil = jiff::fmt::strtime::parse("%a, %d %b %Y %H:%M:%S", body)
        .and_then(|parsed| parsed.to_datetime())
        .map_err(|_| invalid())?;
    ms_of(civil).ok_or_else(|| TimeError::new(format!("{text:?} is before 1970-01-01")))
}

/// Milliseconds at the start of the UTC day and of the UTC month holding
/// `now_ms`.
pub fn period_starts(now_ms: u64) -> (u64, u64) {
    let now = civil_at(now_ms);
    let day = ms_of(now.date().at(0, 0, 0, 0)).unwrap_or(0);
    let month = ms_of(now.first_of_month().date().at(0, 0, 0, 0)).unwrap_or(0);
    (day, month)
}

/// A duration from `500ms`, `30s`, `5m`, `2h`, `7d`, or a bare number of
/// seconds; the number may have a fraction (`1.5h`).
pub fn parse_duration(text: &str) -> Result<Duration, TimeError> {
    let invalid = || TimeError::new(format!("{text:?} is not a duration such as 30s, 5m or 7d"));
    let t = text.trim();
    let split = t
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(t.len());
    let (number, unit) = t.split_at(split);
    let n: f64 = number.parse().map_err(|_| invalid())?;
    let ms = match unit.trim() {
        "ms" => n,
        "" | "s" => n * 1000.0,
        "m" => n * 60_000.0,
        "h" => n * 3_600_000.0,
        "d" => n * 86_400_000.0,
        _ => return Err(invalid()),
    };
    if !(ms.is_finite() && ms >= 0.0) {
        return Err(TimeError::new(format!("{text:?} is not a duration")));
    }
    Ok(Duration::from_millis(ms as u64))
}

/// `30s`, `15m`, `8h` or `7d`: the largest unit that divides `d` into whole
/// seconds, so [`parse_duration`] reads it back.
pub fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        s if s != 0 && s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s != 0 && s % 3600 == 0 => format!("{}h", s / 3600),
        s if s != 0 && s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}
