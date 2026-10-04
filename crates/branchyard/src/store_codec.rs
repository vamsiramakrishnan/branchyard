//! The one place state stores convert between Rust values and database
//! columns, so SQLite and PostgreSQL (and the server's own stores) cannot
//! disagree about what a value means.
//!
//! Both databases keep every number as a signed 64-bit integer, and the
//! code above them keeps unsigned ones. Casting or clamping between them
//! (`as i64`, or `try_from` followed by a defaulting `unwrap_or`) hides a bad value: a
//! timestamp above `i64::MAX` would be stored as the maximum, and a
//! negative one read back as `0`, a PID of `0` is the kernel's reserved
//! one. Use these functions instead. Each takes the column or field name
//! and fails with an error that names it:
//!
//! ```ignore
//! params![name, to_db("created_ms", record.created_ms)?]   // u64 -> i64
//! created_ms: from_db("created_ms", row.get(3)?)?          // i64 -> u64
//! pid: from_db_u32("pid", row.get(6)?)?                    // i64 -> u32
//! until: deadline_capped(now_ms(), wait)                   // a wait that may be "forever"
//! after: parse_text("after", &text)?                       // text -> enum
//! ```
//!
//! `?` converts a [`CodecError`] into [`crate::Error`], `std::io::Error`
//! and `rusqlite::Error`, so these work in engine code, in the services
//! registry and inside row-mapping closures. Enum text comes from
//! `strum` derives on the enum itself (`Display`, `EnumString`), never a
//! hand-written match with a default arm: unknown text on load is an
//! error. CONTRIBUTING.md has the how-to; `tests` below holds the guard
//! that keeps lossy conversions from coming back.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use crate::Error;

/// A value that does not survive the trip between Rust and a column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// A number too large to store in a signed 64-bit column.
    TooLarge { field: &'static str, value: u64 },
    /// A stored number outside the range of the Rust type it loads into
    /// (negative, or wider than `u32`/`u16`).
    OutOfRange {
        field: &'static str,
        value: i64,
        target: &'static str,
    },
    /// Stored text that names no variant of the enum it loads into.
    UnknownText { field: &'static str, text: String },
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::TooLarge { field, value } => {
                write!(f, "{field}: {value} does not fit a signed 64-bit column")
            }
            CodecError::OutOfRange {
                field,
                value,
                target,
            } => write!(f, "{field}: stored {value} is not a valid {target}"),
            CodecError::UnknownText { field, text } => write!(f, "unknown {field} {text:?}"),
        }
    }
}

impl std::error::Error for CodecError {}

impl From<CodecError> for Error {
    fn from(error: CodecError) -> Error {
        Error::State(error.to_string())
    }
}

impl From<CodecError> for std::io::Error {
    fn from(error: CodecError) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, error)
    }
}

impl From<CodecError> for rusqlite::Error {
    fn from(error: CodecError) -> rusqlite::Error {
        rusqlite::Error::ToSqlConversionFailure(Box::new(error))
    }
}

/// `value` as a column value; an error naming `field` above `i64::MAX`.
pub fn to_db(field: &'static str, value: u64) -> Result<i64, CodecError> {
    i64::try_from(value).map_err(|_| CodecError::TooLarge { field, value })
}

/// [`to_db`] for a nullable column.
pub fn to_db_opt(field: &'static str, value: Option<u64>) -> Result<Option<i64>, CodecError> {
    value.map(|v| to_db(field, v)).transpose()
}

/// A count or limit as a column value.
pub fn to_db_usize(field: &'static str, value: usize) -> Result<i64, CodecError> {
    to_db(field, value as u64)
}

/// A row count the driver reports as `u64` (rows affected), as `usize`.
pub fn to_usize(field: &'static str, value: u64) -> Result<usize, CodecError> {
    usize::try_from(value).map_err(|_| CodecError::OutOfRange {
        field,
        value: i64::MAX,
        target: "usize",
    })
}

/// When a lease granted at `now_ms` for `ttl` runs out, in milliseconds.
/// Saturates at `u64::MAX`, which [`to_db`] then refuses, so an absurd
/// TTL is an error where it is stored rather than a wrapped deadline.
pub fn deadline(now_ms: u64, ttl: Duration) -> u64 {
    let ttl_ms = u64::try_from(ttl.as_millis()).map_or(u64::MAX, |ms| ms);
    now_ms.saturating_add(ttl_ms)
}

/// The latest moment a column can hold, in milliseconds: "never".
pub const FOREVER_MS: u64 = i64::MAX as u64;

/// When a wait or limit of `span` starting at `now_ms` runs out, for a
/// deadline whose absurd length means "no deadline in practice" (a `wait`
/// of [`Duration::MAX`], a huge `max_duration`). Unlike [`deadline`] it
/// caps at [`FOREVER_MS`] instead of letting [`to_db`] refuse the value,
/// so the call waits as it was asked to. A lease TTL is not one of these:
/// use [`deadline`], which fails on an absurd TTL.
pub fn deadline_capped(now_ms: u64, span: Duration) -> u64 {
    deadline(now_ms, span).min(FOREVER_MS)
}

/// A duration in milliseconds for scheduling arithmetic, where more than
/// `i64::MAX` just means "effectively forever". Never for a stored value:
/// use [`millis_to_db`], which fails instead.
// An explicit `match`, so the saturation reads as a decision here and no
// `unwrap_or` default is left for the guard below to tell apart.
#[allow(clippy::manual_unwrap_or)]
pub fn millis_saturating(value: Duration) -> i64 {
    match i64::try_from(value.as_millis()) {
        Ok(ms) => ms,
        Err(_) => i64::MAX,
    }
}

/// A duration, in milliseconds, as a column value.
pub fn millis_to_db(field: &'static str, value: Duration) -> Result<i64, CodecError> {
    u64::try_from(value.as_millis())
        .map_err(|_| CodecError::TooLarge {
            field,
            value: u64::MAX,
        })
        .and_then(|ms| to_db(field, ms))
}

/// A stored value as `u64`; an error naming `field` when it is negative.
pub fn from_db(field: &'static str, value: i64) -> Result<u64, CodecError> {
    u64::try_from(value).map_err(|_| CodecError::OutOfRange {
        field,
        value,
        target: "u64",
    })
}

/// [`from_db`] for a nullable column.
pub fn from_db_opt(field: &'static str, value: Option<i64>) -> Result<Option<u64>, CodecError> {
    value.map(|v| from_db(field, v)).transpose()
}

/// A stored value as `u32` (a PID, a count); an error naming `field` when
/// it is negative or above `u32::MAX`.
pub fn from_db_u32(field: &'static str, value: i64) -> Result<u32, CodecError> {
    u32::try_from(value).map_err(|_| CodecError::OutOfRange {
        field,
        value,
        target: "u32",
    })
}

/// A stored value as `i32` (a priority); an error naming `field` when it
/// is outside `i32`.
pub fn from_db_i32(field: &'static str, value: i64) -> Result<i32, CodecError> {
    i32::try_from(value).map_err(|_| CodecError::OutOfRange {
        field,
        value,
        target: "i32",
    })
}

/// A stored `COUNT(*)` or other count as `usize`; an error naming `field`
/// when it is negative or wider than `usize`.
pub fn from_db_usize(field: &'static str, value: i64) -> Result<usize, CodecError> {
    usize::try_from(value).map_err(|_| CodecError::OutOfRange {
        field,
        value,
        target: "usize",
    })
}

/// A stored value as `u16` (a port, an HTTP status); see [`from_db_u32`].
pub fn from_db_u16(field: &'static str, value: i64) -> Result<u16, CodecError> {
    u16::try_from(value).map_err(|_| CodecError::OutOfRange {
        field,
        value,
        target: "u16",
    })
}

/// Stored enum text as the enum (its `strum` `EnumString`); an error
/// naming `field` and the text when no variant has it.
pub fn parse_text<T: FromStr>(field: &'static str, text: &str) -> Result<T, CodecError> {
    text.parse().map_err(|_| CodecError::UnknownText {
        field,
        text: text.to_owned(),
    })
}

/// A value as the JSON text a column keeps.
pub fn encode<T: serde::Serialize>(what: &str, value: &T) -> Result<String, Error> {
    serde_json::to_string(value).map_err(|e| Error::State(format!("encode {what}: {e}")))
}

/// JSON text from a column as a value.
pub fn decode<T: serde::de::DeserializeOwned>(what: &str, text: &str) -> Result<T, Error> {
    serde_json::from_str(text).map_err(|e| Error::State(format!("{what}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_round_trip_and_fail_loudly() {
        assert_eq!(to_db("x", 7), Ok(7));
        assert_eq!(to_db("x", i64::MAX as u64), Ok(i64::MAX));
        let too_large = to_db("created_ms", u64::MAX).unwrap_err();
        assert!(too_large.to_string().contains("created_ms"));
        assert_eq!(from_db("x", 0), Ok(0));
        let negative = from_db("at_ms", -1).unwrap_err();
        assert!(negative.to_string().contains("at_ms"), "{negative}");
        assert_eq!(from_db_u32("pid", 4_294_967_295), Ok(u32::MAX));
        assert!(from_db_u32("pid", 4_294_967_296).is_err());
        assert!(from_db_u32("pid", -1).is_err());
        assert!(from_db_u16("status", 70_000).is_err());
        assert_eq!(to_db_opt("x", None), Ok(None));
        assert!(to_db_opt("x", Some(u64::MAX)).is_err());
        assert_eq!(from_db_opt("x", Some(3)), Ok(Some(3)));
        assert!(from_db_opt("x", Some(-3)).is_err());
        assert!(millis_to_db("x", Duration::MAX).is_err());
        assert_eq!(millis_to_db("x", Duration::from_secs(2)), Ok(2000));
    }

    #[test]
    fn forever_deadlines_cap_where_ttls_still_fail() {
        assert_eq!(deadline_capped(5, Duration::from_secs(1)), 1_005);
        assert_eq!(deadline_capped(5, Duration::MAX), FOREVER_MS);
        assert_eq!(deadline_capped(u64::MAX, Duration::MAX), FOREVER_MS);
        // A capped deadline is always storable.
        assert_eq!(
            to_db("until_ms", deadline_capped(u64::MAX, Duration::MAX)),
            Ok(i64::MAX)
        );
        // A lease TTL is not a wait: an absurd one still fails where it is
        // stored.
        assert!(to_db("lease_until_ms", deadline(5, Duration::MAX)).is_err());
        assert_eq!(from_db_i32("priority", -10), Ok(-10));
        assert!(from_db_i32("priority", i64::from(i32::MAX) + 1).is_err());
        assert_eq!(from_db_usize("count", 3), Ok(3));
        assert!(from_db_usize("count", -1).is_err());
        assert_eq!(to_usize("rows", 3), Ok(3));
    }

    #[test]
    fn errors_convert_for_every_caller() {
        let error = to_db("size", u64::MAX).unwrap_err();
        assert!(matches!(Error::from(error.clone()), Error::State(m) if m.contains("size")));
        let io = std::io::Error::from(error.clone());
        assert_eq!(io.kind(), std::io::ErrorKind::InvalidData);
        assert!(rusqlite::Error::from(error).to_string().contains("size"));
    }

    #[test]
    fn enum_text_is_derived_and_strict() {
        use crate::fleet::{BranchOutcome, TaskKind};
        use crate::graph::After;
        use crate::SteerState;
        use strum::IntoEnumIterator;

        // The text a row stores is the text serde puts on the wire, so a
        // renamed variant cannot change one without the other.
        fn same_as_serde<T>()
        where
            T: IntoEnumIterator
                + fmt::Display
                + serde::Serialize
                + FromStr
                + PartialEq
                + fmt::Debug,
        {
            for variant in T::iter() {
                let json = serde_json::to_value(&variant).unwrap();
                assert_eq!(json, serde_json::Value::String(variant.to_string()));
                assert_eq!(
                    parse_text::<T>("v", &variant.to_string()).ok(),
                    Some(variant)
                );
            }
        }
        same_as_serde::<After>();
        same_as_serde::<BranchOutcome>();
        same_as_serde::<TaskKind>();
        same_as_serde::<crate::models::Api>();
        // `ALL` is the order tables list kinds in; it names every variant.
        assert_eq!(TaskKind::ALL.to_vec(), TaskKind::iter().collect::<Vec<_>>());

        let unknown = parse_text::<After>("after", "eventually").unwrap_err();
        assert_eq!(
            unknown,
            CodecError::UnknownText {
                field: "after",
                text: "eventually".to_owned()
            }
        );
        assert!(parse_text::<BranchOutcome>("outcome", "").is_err());
        assert!(parse_text::<TaskKind>("kind", "Bugfix").is_err());

        // Steer state carries data, so its columns are (text, reason).
        for state in [
            SteerState::Pending,
            SteerState::Delivered,
            SteerState::Accepted,
            SteerState::Refused {
                reason: "why".to_owned(),
            },
        ] {
            let (text, reason) = state.columns();
            let back = SteerState::from_columns(text, reason.map(str::to_owned)).unwrap();
            assert_eq!(back, state);
            let json = serde_json::to_value(&state).unwrap();
            assert_eq!(json["state"], text);
        }
        assert!(SteerState::from_columns("bogus", None).is_err());
    }

    // ---- guard: lossy conversions do not come back ----------------------

    /// Source that must convert through this module. The server's stores
    /// are checked from here because it depends on this crate.
    const GUARDED: &[&str] = &[
        "src/sqlite.rs",
        "src/pg.rs",
        "src/services/sqlite.rs",
        "src/services/pg.rs",
        "../branchyard-server/src/store.rs",
        "../branchyard-server/src/companion/store.rs",
        "../branchyard-server/src/triggers/store.rs",
    ];

    /// Casts and clamps no store may use. A wrapping `as` cast or an
    /// `unwrap_or` default hides a value the column cannot hold.
    const CLAMPS: &[&str] = &[
        concat!("unwrap_or", "(i64::MAX)"),
        concat!("unwrap_or", "(0)"),
        concat!("unwrap_or", "_default()"),
    ];

    fn read(path: &str) -> String {
        let full = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
        std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{}: {e}", full.display()))
    }

    /// Lines of `text` outside `#[cfg(test)]` modules (which sit last).
    fn production(text: &str) -> Vec<(usize, &str)> {
        let end = text.find("#[cfg(test)]").unwrap_or(text.len());
        text[..end]
            .lines()
            .enumerate()
            .map(|(i, l)| (i + 1, l))
            .collect()
    }

    /// Lines converting with `try_from`/`try_into` whose result is defaulted
    /// on the same or the next two lines (a chained call wraps).
    fn clamps(text: &str) -> Vec<String> {
        let lines = production(text);
        let code = |i: usize| {
            lines
                .get(i)
                .map_or("", |(_, l)| l.split("//").next().unwrap_or(""))
        };
        let mut found = Vec::new();
        for (i, (n, line)) in lines.iter().enumerate() {
            let here = code(i);
            if !(here.contains("try_from") || here.contains("try_into")) {
                continue;
            }
            let window = format!("{here} {} {}", code(i + 1), code(i + 2));
            if CLAMPS.iter().any(|c| window.contains(c)) {
                found.push(format!("{n}: {}", line.trim()));
            }
        }
        found
    }

    #[test]
    fn no_store_clamps_a_conversion() {
        let mut all = Vec::new();
        for path in GUARDED {
            for line in clamps(&read(path)) {
                all.push(format!("{path}:{line}"));
            }
        }
        assert!(
            all.is_empty(),
            "convert with crate::store_codec (to_db/from_db/from_db_u32), not try_from + a default:\n{}",
            all.join("\n")
        );
    }

    /// Narrowing and sign-changing `as` casts (`as i64`, `as u64`, `as u32`,
    /// `as u16`, `as i32`, `as usize`) left in the guarded files, per file.
    /// A ratchet: the count may fall, never rise. When you remove a cast,
    /// lower its number here; to add one, convert through
    /// `to_db`/`from_db`/`from_db_u32`/`from_db_u16` instead. A stored PID,
    /// port, turn, count or priority is never one of the casts left.
    const CAST_BUDGET: &[(&str, usize)] = &[
        ("src/sqlite.rs", 0),
        ("src/pg.rs", 0),
        ("src/services/sqlite.rs", 0),
        ("src/services/pg.rs", 0),
        // 6 left, none from a stored value: two clock reads (`now_ms() as
        // i64`), a `usize` rank, `(now - seen).max(0) as u64` (clamped
        // non-negative first), a saturating `f64` cast, and the scheduler's
        // `op.3 as i64` age arithmetic.
        ("../branchyard-server/src/store.rs", 6),
        ("../branchyard-server/src/companion/store.rs", 0),
        ("../branchyard-server/src/triggers/store.rs", 0),
    ];

    /// The target types of the casts the budget counts: the ones that wrap
    /// a stored `i64` into a narrower or unsigned type.
    const CAST_TYPES: &[&str] = &["i64", "u64", "u32", "u16", "i32", "usize"];

    fn casts(text: &str) -> usize {
        production(text)
            .iter()
            .map(|(_, line)| {
                let code = line.split("//").next().unwrap_or("");
                CAST_TYPES
                    .iter()
                    .map(|ty| code.matches(&format!(" as {ty}")).count())
                    .sum::<usize>()
            })
            .sum()
    }

    #[test]
    fn the_cast_count_sees_every_narrowing_type_in_code_only() {
        let source = "let a = pid as u32;\nlet b = n as usize + t as u16 + p as i32;\n\
                      let c = x as i64 + y as u64;\nlet d = z as u8 as char; // q as u32\n\
                      let e = f64::from(1) as f64;\n";
        // Six counted casts; a comment, `as u8` and `as f64` are not.
        assert_eq!(casts(source), 6);
    }

    #[test]
    fn lossy_cast_budget_only_falls() {
        assert_eq!(
            CAST_BUDGET.iter().map(|(p, _)| *p).collect::<Vec<_>>(),
            GUARDED,
            "every guarded file needs a budget"
        );
        let mut problems = Vec::new();
        for (path, budget) in CAST_BUDGET {
            let found = casts(&read(path));
            if found > *budget {
                problems.push(format!(
                    "{path}: {found} `as` casts to {CAST_TYPES:?}, budget {budget}: use store_codec"
                ));
            } else if found < *budget {
                problems.push(format!(
                    "{path}: {found} casts left, budget {budget}: lower CAST_BUDGET to {found}"
                ));
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}
