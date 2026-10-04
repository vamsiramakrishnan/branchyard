//! The one clock's formats against the output of the hand-rolled calendars
//! it replaced. Each row of [`GOLDEN`] was produced by those formatters
//! (Hinnant's `civil_from_days`, pasted into six crates) for the same
//! instant before they were deleted; the new formatters on `jiff` must
//! match them byte for byte, and read them back.

use std::time::Duration;

use branchyard_support::time::{
    amz_date, http_date, human_duration, now_ms, now_nanos, parse_duration, parse_http_date,
    parse_rfc3339, period_starts, rfc3339, rfc3339_secs, system_time_ms, utc_minute,
};
use branchyard_support::{rng, ulid_from_parts};

/// Milliseconds, `rfc3339`, `rfc3339_secs`, `utc_minute`, `amz_date`,
/// `http_date`, the start of the UTC day and the start of the UTC month.
type Row = (
    u64,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    u64,
    u64,
);

const GOLDEN: &[Row] = &[
    (
        0,
        "1970-01-01T00:00:00.000Z",
        "1970-01-01T00:00:00Z",
        "1970-01-01 00:00 UTC",
        "19700101T000000Z",
        "Thu, 01 Jan 1970 00:00:00 GMT",
        0,
        0,
    ),
    (
        1,
        "1970-01-01T00:00:00.001Z",
        "1970-01-01T00:00:00Z",
        "1970-01-01 00:00 UTC",
        "19700101T000000Z",
        "Thu, 01 Jan 1970 00:00:00 GMT",
        0,
        0,
    ),
    (
        999,
        "1970-01-01T00:00:00.999Z",
        "1970-01-01T00:00:00Z",
        "1970-01-01 00:00 UTC",
        "19700101T000000Z",
        "Thu, 01 Jan 1970 00:00:00 GMT",
        0,
        0,
    ),
    (
        86399999,
        "1970-01-01T23:59:59.999Z",
        "1970-01-01T23:59:59Z",
        "1970-01-01 23:59 UTC",
        "19700101T235959Z",
        "Thu, 01 Jan 1970 23:59:59 GMT",
        0,
        0,
    ),
    (
        86400000,
        "1970-01-02T00:00:00.000Z",
        "1970-01-02T00:00:00Z",
        "1970-01-02 00:00 UTC",
        "19700102T000000Z",
        "Fri, 02 Jan 1970 00:00:00 GMT",
        86400000,
        0,
    ),
    (
        31535999999,
        "1970-12-31T23:59:59.999Z",
        "1970-12-31T23:59:59Z",
        "1970-12-31 23:59 UTC",
        "19701231T235959Z",
        "Thu, 31 Dec 1970 23:59:59 GMT",
        31449600000,
        28857600000,
    ),
    (
        68212800000,
        "1972-02-29T12:00:00.000Z",
        "1972-02-29T12:00:00Z",
        "1972-02-29 12:00 UTC",
        "19720229T120000Z",
        "Tue, 29 Feb 1972 12:00:00 GMT",
        68169600000,
        65750400000,
    ),
    (
        946684799999,
        "1999-12-31T23:59:59.999Z",
        "1999-12-31T23:59:59Z",
        "1999-12-31 23:59 UTC",
        "19991231T235959Z",
        "Fri, 31 Dec 1999 23:59:59 GMT",
        946598400000,
        944006400000,
    ),
    (
        946684800000,
        "2000-01-01T00:00:00.000Z",
        "2000-01-01T00:00:00Z",
        "2000-01-01 00:00 UTC",
        "20000101T000000Z",
        "Sat, 01 Jan 2000 00:00:00 GMT",
        946684800000,
        946684800000,
    ),
    (
        951782399999,
        "2000-02-28T23:59:59.999Z",
        "2000-02-28T23:59:59Z",
        "2000-02-28 23:59 UTC",
        "20000228T235959Z",
        "Mon, 28 Feb 2000 23:59:59 GMT",
        951696000000,
        949363200000,
    ),
    (
        951782400000,
        "2000-02-29T00:00:00.000Z",
        "2000-02-29T00:00:00Z",
        "2000-02-29 00:00 UTC",
        "20000229T000000Z",
        "Tue, 29 Feb 2000 00:00:00 GMT",
        951782400000,
        949363200000,
    ),
    (
        951868799999,
        "2000-02-29T23:59:59.999Z",
        "2000-02-29T23:59:59Z",
        "2000-02-29 23:59 UTC",
        "20000229T235959Z",
        "Tue, 29 Feb 2000 23:59:59 GMT",
        951782400000,
        949363200000,
    ),
    (
        951868800000,
        "2000-03-01T00:00:00.000Z",
        "2000-03-01T00:00:00Z",
        "2000-03-01 00:00 UTC",
        "20000301T000000Z",
        "Wed, 01 Mar 2000 00:00:00 GMT",
        951868800000,
        951868800000,
    ),
    (
        1440938160000,
        "2015-08-30T12:36:00.000Z",
        "2015-08-30T12:36:00Z",
        "2015-08-30 12:36 UTC",
        "20150830T123600Z",
        "Sun, 30 Aug 2015 12:36:00 GMT",
        1440892800000,
        1438387200000,
    ),
    (
        1440938160005,
        "2015-08-30T12:36:00.005Z",
        "2015-08-30T12:36:00Z",
        "2015-08-30 12:36 UTC",
        "20150830T123600Z",
        "Sun, 30 Aug 2015 12:36:00 GMT",
        1440892800000,
        1438387200000,
    ),
    (
        1483228799999,
        "2016-12-31T23:59:59.999Z",
        "2016-12-31T23:59:59Z",
        "2016-12-31 23:59 UTC",
        "20161231T235959Z",
        "Sat, 31 Dec 2016 23:59:59 GMT",
        1483142400000,
        1480550400000,
    ),
    (
        1709186828009,
        "2024-02-29T06:07:08.009Z",
        "2024-02-29T06:07:08Z",
        "2024-02-29 06:07 UTC",
        "20240229T060708Z",
        "Thu, 29 Feb 2024 06:07:08 GMT",
        1709164800000,
        1706745600000,
    ),
    (
        1735603200000,
        "2024-12-31T00:00:00.000Z",
        "2024-12-31T00:00:00Z",
        "2024-12-31 00:00 UTC",
        "20241231T000000Z",
        "Tue, 31 Dec 2024 00:00:00 GMT",
        1735603200000,
        1733011200000,
    ),
    (
        1790426096789,
        "2026-09-26T12:34:56.789Z",
        "2026-09-26T12:34:56Z",
        "2026-09-26 12:34 UTC",
        "20260926T123456Z",
        "Sat, 26 Sep 2026 12:34:56 GMT",
        1790380800000,
        1788220800000,
    ),
    (
        1790769601500,
        "2026-09-30T12:00:01.500Z",
        "2026-09-30T12:00:01Z",
        "2026-09-30 12:00 UTC",
        "20260930T120001Z",
        "Wed, 30 Sep 2026 12:00:01 GMT",
        1790726400000,
        1788220800000,
    ),
    (
        1790865000000,
        "2026-10-01T14:30:00.000Z",
        "2026-10-01T14:30:00Z",
        "2026-10-01 14:30 UTC",
        "20261001T143000Z",
        "Thu, 01 Oct 2026 14:30:00 GMT",
        1790812800000,
        1790812800000,
    ),
    (
        1791028800000,
        "2026-10-03T12:00:00.000Z",
        "2026-10-03T12:00:00Z",
        "2026-10-03 12:00 UTC",
        "20261003T120000Z",
        "Sat, 03 Oct 2026 12:00:00 GMT",
        1790985600000,
        1790812800000,
    ),
    (
        1791158399999,
        "2026-10-04T23:59:59.999Z",
        "2026-10-04T23:59:59Z",
        "2026-10-04 23:59 UTC",
        "20261004T235959Z",
        "Sun, 04 Oct 2026 23:59:59 GMT",
        1791072000000,
        1790812800000,
    ),
    (
        1769817600000,
        "2026-01-31T00:00:00.000Z",
        "2026-01-31T00:00:00Z",
        "2026-01-31 00:00 UTC",
        "20260131T000000Z",
        "Sat, 31 Jan 2026 00:00:00 GMT",
        1769817600000,
        1767225600000,
    ),
    (
        1769904000000,
        "2026-02-01T00:00:00.000Z",
        "2026-02-01T00:00:00Z",
        "2026-02-01 00:00 UTC",
        "20260201T000000Z",
        "Sun, 01 Feb 2026 00:00:00 GMT",
        1769904000000,
        1769904000000,
    ),
    (
        1772323200000,
        "2026-03-01T00:00:00.000Z",
        "2026-03-01T00:00:00Z",
        "2026-03-01 00:00 UTC",
        "20260301T000000Z",
        "Sun, 01 Mar 2026 00:00:00 GMT",
        1772323200000,
        1772323200000,
    ),
    (
        2147483647000,
        "2038-01-19T03:14:07.000Z",
        "2038-01-19T03:14:07Z",
        "2038-01-19 03:14 UTC",
        "20380119T031407Z",
        "Tue, 19 Jan 2038 03:14:07 GMT",
        2147472000000,
        2145916800000,
    ),
    (
        2147483648000,
        "2038-01-19T03:14:08.000Z",
        "2038-01-19T03:14:08Z",
        "2038-01-19 03:14 UTC",
        "20380119T031408Z",
        "Tue, 19 Jan 2038 03:14:08 GMT",
        2147472000000,
        2145916800000,
    ),
    (
        3981359593013,
        "2096-02-29T13:13:13.013Z",
        "2096-02-29T13:13:13Z",
        "2096-02-29 13:13 UTC",
        "20960229T131313Z",
        "Wed, 29 Feb 2096 13:13:13 GMT",
        3981312000000,
        3978892800000,
    ),
    (
        4107542399999,
        "2100-02-28T23:59:59.999Z",
        "2100-02-28T23:59:59Z",
        "2100-02-28 23:59 UTC",
        "21000228T235959Z",
        "Sun, 28 Feb 2100 23:59:59 GMT",
        4107456000000,
        4105123200000,
    ),
    (
        4107542400000,
        "2100-03-01T00:00:00.000Z",
        "2100-03-01T00:00:00Z",
        "2100-03-01 00:00 UTC",
        "21000301T000000Z",
        "Mon, 01 Mar 2100 00:00:00 GMT",
        4107542400000,
        4107542400000,
    ),
    (
        4107542400001,
        "2100-03-01T00:00:00.001Z",
        "2100-03-01T00:00:00Z",
        "2100-03-01 00:00 UTC",
        "21000301T000000Z",
        "Mon, 01 Mar 2100 00:00:00 GMT",
        4107542400000,
        4107542400000,
    ),
    (
        4133980799999,
        "2100-12-31T23:59:59.999Z",
        "2100-12-31T23:59:59Z",
        "2100-12-31 23:59 UTC",
        "21001231T235959Z",
        "Fri, 31 Dec 2100 23:59:59 GMT",
        4133894400000,
        4131302400000,
    ),
    (
        4133980800000,
        "2101-01-01T00:00:00.000Z",
        "2101-01-01T00:00:00Z",
        "2101-01-01 00:00 UTC",
        "21010101T000000Z",
        "Sat, 01 Jan 2101 00:00:00 GMT",
        4133980800000,
        4133980800000,
    ),
    (
        7263129600000,
        "2200-02-28T00:00:00.000Z",
        "2200-02-28T00:00:00Z",
        "2200-02-28 00:00 UTC",
        "22000228T000000Z",
        "Fri, 28 Feb 2200 00:00:00 GMT",
        7263129600000,
        7260796800000,
    ),
    (
        7263216000000,
        "2200-03-01T00:00:00.000Z",
        "2200-03-01T00:00:00Z",
        "2200-03-01 00:00 UTC",
        "22000301T000000Z",
        "Sat, 01 Mar 2200 00:00:00 GMT",
        7263216000000,
        7263216000000,
    ),
    (
        13574563200000,
        "2400-02-29T00:00:00.000Z",
        "2400-02-29T00:00:00Z",
        "2400-02-29 00:00 UTC",
        "24000229T000000Z",
        "Tue, 29 Feb 2400 00:00:00 GMT",
        13574563200000,
        13572144000000,
    ),
    (
        13574649600000,
        "2400-03-01T00:00:00.000Z",
        "2400-03-01T00:00:00Z",
        "2400-03-01 00:00 UTC",
        "24000301T000000Z",
        "Wed, 01 Mar 2400 00:00:00 GMT",
        13574649600000,
        13574649600000,
    ),
    (
        32503680000000,
        "3000-01-01T00:00:00.000Z",
        "3000-01-01T00:00:00Z",
        "3000-01-01 00:00 UTC",
        "30000101T000000Z",
        "Wed, 01 Jan 3000 00:00:00 GMT",
        32503680000000,
        32503680000000,
    ),
    (
        64065729600000,
        "4000-02-29T12:00:00.000Z",
        "4000-02-29T12:00:00Z",
        "4000-02-29 12:00 UTC",
        "40000229T120000Z",
        "Tue, 29 Feb 4000 12:00:00 GMT",
        64065686400000,
        64063267200000,
    ),
    (
        95631863430300,
        "5000-06-15T06:30:30.300Z",
        "5000-06-15T06:30:30Z",
        "5000-06-15 06:30 UTC",
        "50000615T063030Z",
        "Sun, 15 Jun 5000 06:30:30 GMT",
        95631840000000,
        95630630400000,
    ),
    (
        221845391999999,
        "8999-12-31T23:59:59.999Z",
        "8999-12-31T23:59:59Z",
        "8999-12-31 23:59 UTC",
        "89991231T235959Z",
        "Tue, 31 Dec 8999 23:59:59 GMT",
        221845305600000,
        221842713600000,
    ),
    (
        221845392000000,
        "9000-01-01T00:00:00.000Z",
        "9000-01-01T00:00:00Z",
        "9000-01-01 00:00 UTC",
        "90000101T000000Z",
        "Wed, 01 Jan 9000 00:00:00 GMT",
        221845392000000,
        221845392000000,
    ),
    (
        253370764800000,
        "9999-01-01T00:00:00.000Z",
        "9999-01-01T00:00:00Z",
        "9999-01-01 00:00 UTC",
        "99990101T000000Z",
        "Fri, 01 Jan 9999 00:00:00 GMT",
        253370764800000,
        253370764800000,
    ),
    (
        253375862399999,
        "9999-02-28T23:59:59.999Z",
        "9999-02-28T23:59:59Z",
        "9999-02-28 23:59 UTC",
        "99990228T235959Z",
        "Sun, 28 Feb 9999 23:59:59 GMT",
        253375776000000,
        253373443200000,
    ),
    (
        253375862400000,
        "9999-03-01T00:00:00.000Z",
        "9999-03-01T00:00:00Z",
        "9999-03-01 00:00 UTC",
        "99990301T000000Z",
        "Mon, 01 Mar 9999 00:00:00 GMT",
        253375862400000,
        253375862400000,
    ),
    (
        253402214399999,
        "9999-12-30T23:59:59.999Z",
        "9999-12-30T23:59:59Z",
        "9999-12-30 23:59 UTC",
        "99991230T235959Z",
        "Thu, 30 Dec 9999 23:59:59 GMT",
        253402128000000,
        253399622400000,
    ),
    (
        253402214400000,
        "9999-12-31T00:00:00.000Z",
        "9999-12-31T00:00:00Z",
        "9999-12-31 00:00 UTC",
        "99991231T000000Z",
        "Fri, 31 Dec 9999 00:00:00 GMT",
        253402214400000,
        253399622400000,
    ),
    (
        253402257600000,
        "9999-12-31T12:00:00.000Z",
        "9999-12-31T12:00:00Z",
        "9999-12-31 12:00 UTC",
        "99991231T120000Z",
        "Fri, 31 Dec 9999 12:00:00 GMT",
        253402214400000,
        253399622400000,
    ),
    (
        253402300799000,
        "9999-12-31T23:59:59.000Z",
        "9999-12-31T23:59:59Z",
        "9999-12-31 23:59 UTC",
        "99991231T235959Z",
        "Fri, 31 Dec 9999 23:59:59 GMT",
        253402214400000,
        253399622400000,
    ),
    (
        253402300799999,
        "9999-12-31T23:59:59.999Z",
        "9999-12-31T23:59:59Z",
        "9999-12-31 23:59 UTC",
        "99991231T235959Z",
        "Fri, 31 Dec 9999 23:59:59 GMT",
        253402214400000,
        253399622400000,
    ),
];

#[test]
fn the_table_covers_the_hard_instants() {
    assert!(GOLDEN.len() >= 50, "{} instants", GOLDEN.len());
    let all: Vec<&str> = GOLDEN.iter().map(|row| row.1).collect();
    for needle in [
        "1970-01-01T00:00:00.000Z",
        "2000-02-29T00:00:00.000Z",
        "2100-02-28T23:59:59.999Z",
        "2100-03-01T00:00:00.000Z",
        "2400-02-29T00:00:00.000Z",
        "9999-12-31T23:59:59.999Z",
    ] {
        assert!(all.contains(&needle), "the table lacks {needle}");
    }
}

#[test]
fn every_format_matches_the_old_calendars() {
    for &(ms, millis, secs, minute, amz, http, day, month) in GOLDEN {
        assert_eq!(rfc3339(ms), millis, "rfc3339({ms})");
        assert_eq!(rfc3339_secs(ms), secs, "rfc3339_secs({ms})");
        assert_eq!(utc_minute(ms), minute, "utc_minute({ms})");
        assert_eq!(amz_date(ms), amz, "amz_date({ms})");
        assert_eq!(http_date(ms), http, "http_date({ms})");
        assert_eq!(period_starts(ms), (day, month), "period_starts({ms})");
    }
}

#[test]
fn every_format_reads_back() {
    for &(ms, millis, secs, _, _, http, _, _) in GOLDEN {
        assert_eq!(parse_rfc3339(millis), Ok(ms), "{millis}");
        assert_eq!(parse_rfc3339(secs), Ok(ms - ms % 1000), "{secs}");
        assert_eq!(parse_http_date(http), Ok(ms - ms % 1000), "{http}");
    }
}

#[test]
fn rfc3339_reads_offsets_fractions_and_whitespace() {
    let noon = 1_790_769_601_500;
    assert_eq!(parse_rfc3339("2026-09-30T12:00:01.5Z"), Ok(noon));
    assert_eq!(parse_rfc3339("2026-09-30T12:00:01.500999999Z"), Ok(noon));
    assert_eq!(parse_rfc3339("2026-09-30t12:00:01.5z"), Ok(noon));
    assert_eq!(parse_rfc3339("2026-09-30T14:00:01.5+02:00"), Ok(noon));
    assert_eq!(parse_rfc3339("2026-09-30T07:00:01.5-05:00"), Ok(noon));
    assert_eq!(parse_rfc3339("  2026-09-30T12:00:01.5Z\n"), Ok(noon));
    // The AWS test suite's date.
    assert_eq!(parse_rfc3339("2015-08-30T12:36:00Z"), Ok(1_440_938_160_000));
}

#[test]
fn rfc3339_refuses_what_is_not_a_time() {
    for bad in [
        "",
        "yesterday",
        "2026-13-01T00:00:00Z",
        "2026-00-10T00:00:00Z",
        "2026-02-30T00:00:00Z",
        "2026-02-29T00:00:00Z",
        "2026-09-30T24:00:00Z",
        "2026-09-30T23:60:00Z",
        "2026-09-30T24:00:61Z",
        "2026-09-30T12:00:00",
        "2026-09-30T12:00",
        "2026-09-30",
        "2026-09-30 12:00:00Z",
        "2026-09-30T12:00:00Zjunk",
        "2026-09-30T12:00:00Z[UTC]",
        "2026-09-30T12:00:00+25:00",
        "1969-12-31T23:59:59Z",
        "-001-01-01T00:00:00Z",
    ] {
        let error = parse_rfc3339(bad).expect_err(bad);
        assert!(!error.to_string().is_empty(), "{bad:?}");
    }
}

#[test]
fn http_dates_refuse_what_is_not_one() {
    for bad in [
        "",
        "Sun, 30 Aug 2015 12:36:00",
        "Sun, 30 Foo 2015 12:36:00 GMT",
        "Sun, 31 Sep 2015 12:36:00 GMT",
        "Sun, 30 Aug 2015 25:36:00 GMT",
        "2015-08-30T12:36:00Z",
    ] {
        assert!(parse_http_date(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_time_past_year_9999_is_written_as_the_last_instant() {
    assert_eq!(rfc3339(u64::MAX), "9999-12-31T23:59:59.999Z");
    assert_eq!(rfc3339_secs(u64::MAX), "9999-12-31T23:59:59Z");
    assert_eq!(utc_minute(u64::MAX), "9999-12-31 23:59 UTC");
}

#[test]
fn durations_read_units_and_write_the_largest() {
    assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
    assert_eq!(parse_duration("30"), Ok(Duration::from_secs(30)));
    assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
    assert_eq!(parse_duration(" 5m "), Ok(Duration::from_secs(300)));
    assert_eq!(parse_duration("1.5h"), Ok(Duration::from_secs(5400)));
    assert_eq!(parse_duration("7d"), Ok(Duration::from_secs(604_800)));
    for bad in ["", "m", "5x", "-5s", "1e3", "5 minutes", "NaN"] {
        assert!(parse_duration(bad).is_err(), "{bad:?}");
    }
    for (d, text) in [
        (Duration::from_secs(30), "30s"),
        (Duration::from_secs(900), "15m"),
        (Duration::from_secs(8 * 3600), "8h"),
        (Duration::from_secs(7 * 86_400), "7d"),
        (Duration::from_secs(90), "90s"),
        (Duration::ZERO, "0s"),
    ] {
        assert_eq!(human_duration(d), text);
        assert_eq!(parse_duration(text), Ok(d));
    }
}

#[test]
fn the_clock_reads_now() {
    let before = now_ms();
    let nanos = now_nanos();
    let after = now_ms();
    // After 2026-01-01, so not the epoch fallback.
    assert!(before > 1_767_225_600_000, "{before}");
    assert!(before <= after);
    assert!((nanos / 1_000_000) as u64 >= before - 1);
    assert_eq!(system_time_ms(std::time::UNIX_EPOCH), Some(0));
    assert_eq!(
        system_time_ms(std::time::UNIX_EPOCH - Duration::from_secs(1)),
        None
    );
}

#[test]
fn period_starts_are_the_utc_day_and_month() {
    // 2026-10-03T12:00:00Z.
    let (day, month) = period_starts(1_790_985_600_000);
    assert_eq!(rfc3339_secs(day), "2026-10-03T00:00:00Z");
    assert_eq!(rfc3339_secs(month), "2026-10-01T00:00:00Z");
}

#[test]
fn ulids_are_the_crockford_encoding_of_time_then_random() {
    // The hand-rolled encoder this replaced, on fixed parts.
    fn old(ms: u64, random: [u8; 10]) -> String {
        const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
        let mut bytes = [0u8; 16];
        bytes[6..].copy_from_slice(&random);
        let value = ((ms as u128 & ((1 << 48) - 1)) << 80) | u128::from_be_bytes(bytes);
        (0..26)
            .map(|i| ALPHABET[((value >> (125 - 5 * i)) & 31) as usize] as char)
            .collect()
    }
    for (ms, random) in [
        (0, [0u8; 10]),
        (1, [0xff; 10]),
        (1_791_028_800_000, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
        ((1 << 48) - 1, [0xa5; 10]),
        (u64::MAX, [0x5a; 10]),
    ] {
        assert_eq!(ulid_from_parts(ms, random), old(ms, random), "{ms}");
    }
    let a = branchyard_support::new_ulid().unwrap();
    // `a` was made no later than this millisecond; `b` is made in a later one.
    let made = branchyard_support::time::now_ms();
    branchyard_testkit::wait::until("the clock to pass the first ULID's millisecond", || {
        branchyard_support::time::now_ms() > made
    });
    let b = branchyard_support::new_ulid().unwrap();
    assert_eq!(a.len(), 26);
    assert!(a < b, "{a} {b}");
}

#[test]
fn splitmix64_repeats_forever() {
    // The first outputs of the reference SplitMix64 for seed 0, and for the
    // seed `tasks::large` builds its chunking table from.
    let mut zero = rng::SplitMix64::new(0);
    assert_eq!(zero.next_u64(), 0xe220_a839_7b1d_cdaf);
    assert_eq!(zero.next_u64(), 0x6e78_9e6a_a1b9_65f4);
    let mut again = rng::SplitMix64::new(0);
    assert_eq!(again.next_u64(), 0xe220_a839_7b1d_cdaf);
    let (state, word) = rng::splitmix64(0);
    assert_eq!(
        (state, word),
        (0x9e37_79b9_7f4a_7c15, 0xe220_a839_7b1d_cdaf)
    );
    let mut below = rng::SplitMix64::new(7);
    for _ in 0..100 {
        assert!(below.below(10) < 10);
        assert!(below.below_or_at(3) <= 3);
        let u = below.uniform();
        assert!(u > 0.0 && u < 1.0);
    }
    assert_eq!(below.below(0), 0);
}

#[test]
fn entropy_is_not_constant() {
    let mut a = [0u8; 32];
    let mut b = [0u8; 32];
    rng::fill_random(&mut a).unwrap();
    rng::fill_random(&mut b).unwrap();
    assert_ne!(a, b);
    assert_ne!(rng::fresh_seed(), rng::fresh_seed());
    assert_ne!(rng::entropy_seed().unwrap(), rng::entropy_seed().unwrap());
}
