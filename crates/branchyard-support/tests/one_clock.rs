//! The guard: the clock, the calendar and the random generator each have one
//! home, `branchyard_support::{time, rng}`. This fails when a crate writes
//! its own again, in any `crates/*/{src,tests,examples,benches}` file:
//!
//! - the calendar and SplitMix64 magic numbers (Hinnant's `civil_from_days`
//!   was pasted into six crates, SplitMix64 into six more);
//! - a function named `now_ms`, `now_secs`, `unix_now` and the like (there
//!   were fourteen); exactly one `fn now_ms` exists, in `time.rs`;
//! - `SystemTime::now()` or `UNIX_EPOCH`, which is how each of them read the
//!   clock and swallowed a clock set before 1970 with `unwrap_or(0)`.
//!
//! Use `branchyard_support::time` (`now_ms`, `rfc3339`, `parse_rfc3339`,
//! `http_date`, `period_starts`, `parse_duration`), `rng::SplitMix64` and
//! `new_ulid`. See CONTRIBUTING.md, "Time, ids and randomness". This runs
//! with the crate's tests in CI.

#![allow(clippy::unwrap_used)] // tests: a panic is the failure report
use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Numbers only the shared module may spell: Hinnant's days-from-civil
/// offsets and the SplitMix64 constants, lower case with `_` removed.
const MAGIC: [&str; 5] = [
    "719468",
    "146097",
    "0x9e3779b97f4a7c15",
    "0xbf58476d1ce4e5b9",
    "0x94d049bb133111eb",
];

/// Names of functions that read the clock or split a date by hand.
const BANNED_FNS: [&str; 7] = [
    "now_secs",
    "unix_now",
    "now_millis",
    "now_unix",
    "now_nanos",
    "civil_from_days",
    "days_from_civil",
];

/// What is wrong with `source`, one line each.
pub fn violations(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    // Each literal-looking token, spelled without `_` and in lower case, so
    // `0x9E37_79B9_7F4A_7C15` and `719_468` match however they are grouped
    // and `7194680` does not.
    let mut tokens = std::collections::BTreeSet::new();
    for token in source.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        if token.starts_with(|c: char| c.is_ascii_digit()) {
            tokens.insert(token.replace('_', "").to_ascii_lowercase());
        }
    }
    for magic in MAGIC {
        if tokens.contains(magic) {
            found.push(format!("spells {magic}"));
        }
    }
    for line in source.lines() {
        let code = line.split("//").next().unwrap_or("");
        for name in BANNED_FNS.iter().chain(["now_ms"].iter()) {
            if has_fn(code, name) {
                found.push(format!("defines fn {name}"));
            }
        }
        if code.contains("SystemTime::now") {
            found.push("reads SystemTime::now".into());
        }
        if code.contains("UNIX_EPOCH") {
            found.push("names UNIX_EPOCH".into());
        }
    }
    found.sort();
    found.dedup();
    found
}

/// Whether `code` declares a function called `name`, as a free function or
/// a method.
fn has_fn(code: &str, name: &str) -> bool {
    let mut rest = code;
    while let Some(at) = rest.find("fn ") {
        let before_ok = at == 0 || !rest.as_bytes()[at - 1].is_ascii_alphanumeric();
        let after = &rest[at + 3..];
        let ident: String = after
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if before_ok && ident == name {
            return true;
        }
        rest = after;
    }
    false
}

fn workspace_files() -> (PathBuf, Vec<PathBuf>) {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .unwrap();
    let mut files = Vec::new();
    for entry in fs::read_dir(&crates).unwrap().flatten() {
        for dir in ["src", "tests", "examples", "benches"] {
            rust_files(&entry.path().join(dir), &mut files);
        }
    }
    assert!(
        files.len() > 100,
        "the scan found only {} files",
        files.len()
    );
    (crates, files)
}

/// Whether `path` (under `crates`) is in this crate, which defines all of it.
fn is_this_crate(crates: &Path, path: &Path) -> bool {
    path.strip_prefix(crates)
        .is_ok_and(|rel| rel.starts_with("branchyard-support"))
}

#[test]
fn no_crate_keeps_its_own_clock_calendar_or_generator() {
    let (crates, files) = workspace_files();
    let mut offenders = Vec::new();
    for file in files.iter().filter(|f| !is_this_crate(&crates, f)) {
        let found = violations(&fs::read_to_string(file).unwrap());
        if !found.is_empty() {
            offenders.push(format!(
                "{}: {}",
                file.strip_prefix(&crates).unwrap().display(),
                found.join(", ")
            ));
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "use branchyard_support::time (now_ms, rfc3339, parse_rfc3339, http_date, \
         period_starts, parse_duration) and branchyard_support::rng instead of \
         your own clock, calendar or SplitMix64 in:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn there_is_exactly_one_now_ms() {
    let (crates, files) = workspace_files();
    let mut homes: Vec<String> = files
        .iter()
        .filter(|f| !f.ends_with("tests/one_clock.rs"))
        .filter(|f| {
            fs::read_to_string(f)
                .unwrap()
                .lines()
                .any(|l| has_fn(l.split("//").next().unwrap_or(""), "now_ms"))
        })
        .map(|f| f.strip_prefix(&crates).unwrap().display().to_string())
        .collect();
    homes.sort();
    assert_eq!(
        homes,
        ["branchyard-support/src/time.rs"],
        "`fn now_ms` is defined once, in branchyard-support/src/time.rs"
    );
}

#[test]
fn the_detector_sees_each_way_to_roll_your_own() {
    for (bad, what) in [
        ("let z = days + 719_468;", "719468"),
        ("let era = z.div_euclid(146_097);", "146097"),
        (
            "state.wrapping_add(0x9E37_79B9_7F4A_7C15)",
            "0x9e3779b97f4a7c15",
        ),
        ("wrapping_mul(0xbf58_476d_1ce4_e5b9)", "0xbf58476d1ce4e5b9"),
        ("fn now_ms() -> u64 {", "defines fn now_ms"),
        ("pub(crate) fn now_ms() -> u64 {", "defines fn now_ms"),
        ("    pub fn now_ms(&self) -> i64 {", "defines fn now_ms"),
        ("fn now_secs() -> u64 {", "defines fn now_secs"),
        ("fn unix_now() -> Duration {", "defines fn unix_now"),
        (
            "fn days_from_civil(y: i64) -> i64 {",
            "defines fn days_from_civil",
        ),
        (
            "SystemTime::now().duration_since(x)",
            "reads SystemTime::now",
        ),
        ("std::time::SystemTime::now()", "reads SystemTime::now"),
        (".duration_since(UNIX_EPOCH)", "names UNIX_EPOCH"),
        (
            "t.duration_since(std::time::UNIX_EPOCH)",
            "names UNIX_EPOCH",
        ),
    ] {
        let found = violations(bad);
        assert!(
            found.iter().any(|f| f.contains(what)),
            "missed {bad:?}: {found:?}"
        );
    }
    for fine in [
        "let now = now_ms();",
        "use branchyard_support::time::now_ms;",
        "fn now_ms_of(x: u64) -> u64 { x }",
        "fn known_ms() {}",
        "// fn now_ms() was here, now SystemTime::now and UNIX_EPOCH are gone",
        "let millis = 1_000_000_000_000;",
        "let n = 7194680;",
    ] {
        assert!(violations(fine).is_empty(), "flagged {fine:?}");
    }
}
