//! Five-field cron expressions, read in a time zone.
//!
//! No cron crate is in `Cargo.lock`, so this is a small parser for the
//! common Vixie syntax: `minute hour day-of-month month day-of-week`, each
//! field `*`, a number, a range `a-b`, a step `*/n`, `a-b/n` or `a/n`, or a
//! comma-separated list of those; month and weekday names (`jan`, `mon`);
//! `0` or `7` for Sunday; and `@hourly`, `@daily` (`@midnight`),
//! `@weekly`, `@monthly`, `@yearly` (`@annually`). When both day fields
//! are restricted, a day matches either (Vixie's rule). `L`, `W`, `#`,
//! `?` and seconds are refused.
//!
//! Times are civil times in the expression's time zone (via `jiff`): a
//! civil time that does not exist because clocks moved forward fires at
//! the same distance past the gap's start as it was meant to be (02:30 on
//! a night that skips 02:00 to 03:00 fires at 03:30), and one that occurs
//! twice because clocks moved back fires once, at its first occurrence.

use jiff::civil::{Date, DateTime, Time};
use jiff::tz::TimeZone;
use jiff::{Timestamp, ToSpan};

/// A parsed expression with its time zone.
#[derive(Clone, Debug)]
pub struct Cron {
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
    /// Day of month was `*` (or `*/1`).
    any_day: bool,
    /// Day of week was `*`.
    any_weekday: bool,
    tz: TimeZone,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// How far ahead [`Cron::next_after`] looks before it says an expression
/// never matches (`0 0 30 2 *`).
const HORIZON_YEARS: i16 = 8;

fn field_value(text: &str, names: &[&str], offset: u32) -> Option<u32> {
    if let Ok(n) = text.parse::<u32>() {
        return Some(n);
    }
    let lower = text.to_ascii_lowercase();
    names
        .iter()
        .position(|n| *n == lower)
        .map(|i| i as u32 + offset)
}

/// One field as a bit set over `min..=max`, and whether it was `*`.
fn field(
    text: &str,
    what: &str,
    min: u32,
    max: u32,
    names: &[&str],
    name_offset: u32,
) -> Result<(u64, bool), String> {
    if text.is_empty() {
        return Err(format!("the {what} field is empty"));
    }
    let mut bits = 0u64;
    let mut star = false;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step
                    .parse()
                    .ok()
                    .filter(|s| *s > 0)
                    .ok_or_else(|| format!("{part:?} in the {what} field has a bad step"))?;
                (range, Some(step))
            }
            None => (part, None),
        };
        let bad = || format!("{part:?} is not a {what} ({min} to {max})");
        let (lo, hi) = if range == "*" {
            if step.is_none_or(|s| s == 1) && text.split(',').count() == 1 {
                star = true;
            }
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let a = field_value(a, names, name_offset).ok_or_else(bad)?;
            let b = field_value(b, names, name_offset).ok_or_else(bad)?;
            (a, b)
        } else {
            let a = field_value(range, names, name_offset).ok_or_else(bad)?;
            // `a/n` means from a to the end, stepping n.
            (a, if step.is_some() { max } else { a })
        };
        if lo < min || hi > max || lo > hi {
            return Err(bad());
        }
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v += step.unwrap_or(1);
        }
    }
    Ok((bits, star))
}

fn has(bits: u64, v: i64) -> bool {
    (0..64).contains(&v) && bits & (1 << v) != 0
}

impl Cron {
    /// Parse `expr` read in `timezone` (an IANA name such as
    /// `Europe/Berlin`, or `UTC`).
    pub fn parse(expr: &str, timezone: &str) -> Result<Cron, String> {
        let tz = match timezone {
            "UTC" | "utc" | "Etc/UTC" => TimeZone::UTC,
            name => TimeZone::get(name)
                .map_err(|e| format!("{name:?} is not a time zone this host knows: {e}"))?,
        };
        let expr = expr.trim();
        let expanded = match expr.to_ascii_lowercase().as_str() {
            "@yearly" | "@annually" => "0 0 1 1 *".to_owned(),
            "@monthly" => "0 0 1 * *".to_owned(),
            "@weekly" => "0 0 * * 0".to_owned(),
            "@daily" | "@midnight" => "0 0 * * *".to_owned(),
            "@hourly" => "0 * * * *".to_owned(),
            other if other.starts_with('@') => {
                return Err(format!("{expr:?} is not a cron shorthand"));
            }
            _ => expr.to_owned(),
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "{expr:?} has {} fields; a cron expression has five: minute hour day-of-month \
                 month day-of-week",
                fields.len()
            ));
        }
        let (minutes, _) = field(fields[0], "minute", 0, 59, &[], 0)?;
        let (hours, _) = field(fields[1], "hour", 0, 23, &[], 0)?;
        let (days, any_day) = field(fields[2], "day of month", 1, 31, &[], 0)?;
        let (months, _) = field(fields[3], "month", 1, 12, &MONTHS, 1)?;
        let (mut weekdays, any_weekday) = field(fields[4], "day of week", 0, 7, &WEEKDAYS, 0)?;
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays | 1) & !(1 << 7);
        }
        let cron = Cron {
            minutes,
            hours,
            days,
            months,
            weekdays,
            any_day,
            any_weekday,
            tz,
        };
        if cron.next_after(0).is_none() {
            return Err(format!("{expr:?} never matches a date"));
        }
        Ok(cron)
    }

    fn day_matches(&self, date: Date) -> bool {
        let dom = has(self.days, i64::from(date.day()));
        let dow = has(
            self.weekdays,
            i64::from(date.weekday().to_sunday_zero_offset()),
        );
        match (self.any_day, self.any_weekday) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        }
    }

    /// The first matching time strictly after `after_ms` (milliseconds
    /// since the Unix epoch), in milliseconds; `None` when nothing matches
    /// within eight years.
    pub fn next_after(&self, after_ms: u64) -> Option<u64> {
        let after = Timestamp::from_millisecond(i64::try_from(after_ms).ok()?).ok()?;
        let zoned = after.to_zoned(self.tz.clone());
        // The next whole minute, as a civil time.
        let start = zoned
            .datetime()
            .with()
            .second(0)
            .subsec_nanosecond(0)
            .build()
            .ok()?;
        let mut t = start.checked_add(1.minute()).ok()?;
        let limit = Date::new(zoned.year().saturating_add(HORIZON_YEARS), 1, 1).ok()?;
        while t.date() < limit {
            if !has(self.months, i64::from(t.month())) {
                // The first day of the next month.
                let first = t.date().first_of_month().checked_add(1.month()).ok()?;
                t = DateTime::from_parts(first, Time::midnight());
                continue;
            }
            if !self.day_matches(t.date()) {
                t = DateTime::from_parts(t.date().tomorrow().ok()?, Time::midnight());
                continue;
            }
            if !has(self.hours, i64::from(t.hour())) {
                t = t
                    .with()
                    .minute(0)
                    .build()
                    .ok()?
                    .checked_add(1.hour())
                    .ok()?;
                continue;
            }
            if !has(self.minutes, i64::from(t.minute())) {
                t = t.checked_add(1.minute()).ok()?;
                continue;
            }
            // A civil time in a gap maps forward; one in a fold to its
            // first instant. Either may land at or before `after` (a
            // fold's earlier hour already passed): look further then.
            let instant = self.tz.to_zoned(t).ok()?.timestamp();
            if instant > after {
                return u64::try_from(instant.as_millisecond()).ok();
            }
            t = t.checked_add(1.minute()).ok()?;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(text: &str) -> u64 {
        text.parse::<Timestamp>().unwrap().as_millisecond() as u64
    }

    fn at(ms: u64) -> String {
        Timestamp::from_millisecond(ms as i64).unwrap().to_string()
    }

    fn next(expr: &str, tz: &str, after: &str) -> String {
        at(Cron::parse(expr, tz)
            .unwrap()
            .next_after(ms(after))
            .unwrap())
    }

    #[test]
    fn fields_steps_ranges_lists_and_names() {
        assert_eq!(
            next("*/15 * * * *", "UTC", "2026-10-01T10:07:30Z"),
            "2026-10-01T10:15:00Z"
        );
        assert_eq!(
            next("0 9 * * mon-fri", "UTC", "2026-10-02T09:00:00Z"),
            "2026-10-05T09:00:00Z",
            "Friday 9:00 is not after itself; next is Monday"
        );
        assert_eq!(
            next("30 4 1,15 * *", "UTC", "2026-10-01T05:00:00Z"),
            "2026-10-15T04:30:00Z"
        );
        assert_eq!(
            next("0 0 1 jan *", "UTC", "2026-10-01T00:00:00Z"),
            "2027-01-01T00:00:00Z"
        );
        assert_eq!(
            next("5/20 * * * *", "UTC", "2026-10-01T10:46:00Z"),
            "2026-10-01T11:05:00Z"
        );
        assert_eq!(
            next("0 12 * * 7", "UTC", "2026-10-01T00:00:00Z"),
            "2026-10-04T12:00:00Z",
            "7 is Sunday"
        );
        assert_eq!(
            next("@hourly", "UTC", "2026-10-01T10:00:00Z"),
            "2026-10-01T11:00:00Z"
        );
        assert_eq!(
            next("@monthly", "UTC", "2026-12-15T00:00:00Z"),
            "2027-01-01T00:00:00Z"
        );
    }

    #[test]
    fn restricted_day_fields_match_either() {
        // The 13th, or any Friday.
        assert_eq!(
            next("0 0 13 * fri", "UTC", "2026-10-01T00:00:00Z"),
            "2026-10-02T00:00:00Z"
        );
        assert_eq!(
            next("0 0 13 * fri", "UTC", "2026-10-10T00:00:00Z"),
            "2026-10-13T00:00:00Z"
        );
    }

    #[test]
    fn leap_days_and_impossible_dates() {
        assert_eq!(
            next("0 0 29 2 *", "UTC", "2026-03-01T00:00:00Z"),
            "2028-02-29T00:00:00Z"
        );
        let err = Cron::parse("0 0 30 2 *", "UTC").unwrap_err();
        assert!(err.contains("never matches"), "{err}");
    }

    #[test]
    fn time_zones_and_daylight_saving() {
        // 9:00 in New York is 13:00 UTC in summer, 14:00 in winter.
        assert_eq!(
            next("0 9 * * *", "America/New_York", "2026-10-01T00:00:00Z"),
            "2026-10-01T13:00:00Z"
        );
        assert_eq!(
            next("0 9 * * *", "America/New_York", "2026-12-01T00:00:00Z"),
            "2026-12-01T14:00:00Z"
        );
        // 2026-03-08: 02:30 does not exist; it fires at 03:30 EDT (07:30Z).
        assert_eq!(
            next("30 2 * * *", "America/New_York", "2026-03-08T00:00:00Z"),
            "2026-03-08T07:30:00Z"
        );
        // 2026-11-01: 01:30 happens twice; it fires once, at the first.
        let cron = Cron::parse("30 1 * * *", "America/New_York").unwrap();
        let first = cron.next_after(ms("2026-11-01T00:00:00Z")).unwrap();
        assert_eq!(at(first), "2026-11-01T05:30:00Z");
        let second = cron.next_after(first).unwrap();
        assert_eq!(at(second), "2026-11-02T06:30:00Z");
    }

    #[test]
    fn refusals_name_the_problem() {
        for (expr, tz, needle) in [
            ("* * * *", "UTC", "five"),
            ("60 * * * *", "UTC", "minute"),
            ("* 24 * * *", "UTC", "hour"),
            ("* * 0 * *", "UTC", "day of month"),
            ("* * * 13 *", "UTC", "month"),
            ("* * * * 8", "UTC", "day of week"),
            ("*/0 * * * *", "UTC", "step"),
            ("5-1 * * * *", "UTC", "minute"),
            ("@reboot", "UTC", "shorthand"),
            ("0 0 L * *", "UTC", "day of month"),
            ("* * * * *", "Mars/Olympus", "time zone"),
        ] {
            let err = Cron::parse(expr, tz).unwrap_err();
            assert!(err.contains(needle), "{expr}: {err}");
        }
    }
}
