"""What the README screenshots show: a small Rust library with branches in every lane, and a
knowledge project whose sessions turn two customer exports into tables.

Everything here is demo data. The scripted agent (scripted_agent.py) plays each turn: it writes
the turn's files, reports its tool calls and answers with its reply. A turn without a reply keeps
running; its branch's `fate` says what generate.py does with it after that.
"""

from __future__ import annotations

import csv
import io
import json
from collections import Counter
from collections.abc import Iterable
from dataclasses import dataclass, field
from typing import Any, Literal

Fate = Literal["merged", "interrupted", "running", "ready"]


@dataclass(frozen=True)
class Turn:
    prompt: str
    reply: str | None
    say: str | None = None
    tools: tuple[tuple[str, str], ...] = ()
    """(ACP tool kind, title) for each call, in order."""
    files: dict[str, str] = field(default_factory=dict)
    outputs: dict[str, str] = field(default_factory=dict)
    """CSV files the turn publishes, by name under outputs/."""


@dataclass(frozen=True)
class Branch:
    name: str
    fate: Fate
    turn: Turn


@dataclass(frozen=True)
class Session:
    name: str
    collect: bool
    turns: tuple[Turn, ...]


REPO = "logfmt"
OPEN_BRANCH = "parser-fix"

CARGO_TOML = """\
[package]
name = "logfmt"
version = "0.4.2"
edition = "2021"
description = "Parse and print logfmt lines"
license = "MIT"

[dependencies]
"""

README = """\
# logfmt

Parse and print [logfmt](https://brandur.org/logfmt) lines, such as
`level=info msg="server started" port=8080`.
"""

LIB_RS = """\
//! Parse and print logfmt lines, such as `level=info msg="server started"`.

mod parse;
mod record;

pub use parse::{parse, ParseError};
pub use record::Record;
"""

PARSE_RS = """\
use crate::Record;

/// Why a line is not logfmt.
#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// A key with no `=value` after it.
    MissingValue(String),
    /// A quoted value with no closing quote.
    UnclosedQuote(String),
}

/// Parse one line of `key=value` pairs.
pub fn parse(line: &str) -> Result<Record, ParseError> {
    let mut record = Record::default();
    let mut rest = line.trim();
    while !rest.is_empty() {
        let (key, after) = rest
            .split_once('=')
            .ok_or_else(|| ParseError::MissingValue(rest.to_owned()))?;
        let (value, after) = match after.strip_prefix('"') {
            Some(quoted) => {
                let end = quoted
                    .find('"')
                    .ok_or_else(|| ParseError::UnclosedQuote(key.to_owned()))?;
                (&quoted[..end], &quoted[end + 1..])
            }
            None => after.split_once(' ').unwrap_or((after, "")),
        };
        record.insert(key, value);
        rest = after.trim_start();
    }
    Ok(record)
}
"""

RECORD_HEAD = """\
use std::collections::HashMap;
use std::fmt;

/// One logfmt line: its fields by key.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Record {
    fields: HashMap<String, String>,
}

impl Record {
    pub fn insert(&mut self, key: &str, value: &str) {
        self.fields.insert(key.to_owned(), value.to_owned());
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }
"""

RECORD_FIXED_HEAD = """\
use std::fmt;

/// One logfmt line, its fields in line order.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Record {
    fields: Vec<(String, String)>,
}

impl Record {
    /// A repeated key keeps its first place.
    pub fn insert(&mut self, key: &str, value: &str) {
        let value = value.to_owned();
        match self.position(key) {
            Some(i) => self.fields[i].1 = value,
            None => self.fields.push((key.into(), value)),
        }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        let i = self.position(key)?;
        Some(&self.fields[i].1)
    }

    fn position(&self, key: &str) -> Option<usize> {
        self.fields.iter().position(|(k, _)| k == key)
    }
"""

RECORD_TAIL = """
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

impl fmt::Display for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, (key, value)) in self.fields.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            if value.contains(' ') {
                write!(f, "{key}=\\"{value}\\"")?;
            } else {
                write!(f, "{key}={value}")?;
            }
        }
        Ok(())
    }
}
"""

TESTS_HEAD = """\
use logfmt::{parse, ParseError};

#[test]
fn reads_bare_and_quoted_values() {
    let record = parse(r#"level=info msg="server started" port=8080"#).unwrap();
    assert_eq!(record.get("level"), Some("info"));
    assert_eq!(record.get("msg"), Some("server started"));
    assert_eq!(record.get("port"), Some("8080"));
}

#[test]
fn prints_a_line_back() {
    let line = r#"level=info msg="server started""#;
    assert_eq!(parse(line).unwrap().to_string(), line);
}
"""

TESTS_ADDED = """
#[test]
fn keeps_field_order_on_every_run() {
    let line = "ts=09:12:44 level=warn msg=retrying attempt=3 host=api-2";
    for _ in 0..100 {
        assert_eq!(parse(line).unwrap().to_string(), line);
    }
}

#[test]
fn a_repeated_key_keeps_its_place() {
    let record = parse("a=1 b=2 a=3").unwrap();
    assert_eq!(record.to_string(), "a=3 b=2");
}
"""

TESTS_TAIL = """
#[test]
fn rejects_a_key_without_a_value() {
    let error = parse("level=info msg").unwrap_err();
    assert_eq!(error, ParseError::MissingValue("msg".into()));
}

#[test]
fn rejects_an_unclosed_quote() {
    let error = parse(r#"msg="server"#).unwrap_err();
    assert_eq!(error, ParseError::UnclosedQuote("msg".into()));
}
"""

REPO_FILES = {
    ".gitignore": "/target\n",
    "Cargo.toml": CARGO_TOML,
    "README.md": README,
    "src/lib.rs": LIB_RS,
    "src/parse.rs": PARSE_RS,
    "src/record.rs": RECORD_HEAD + RECORD_TAIL,
    "tests/parser.rs": TESTS_HEAD + TESTS_TAIL,
}

CI_YML = """\
name: ci
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        rust: ["1.74", stable]
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@master
        with:
          toolchain: ${{ matrix.rust }}
      - run: cargo test --locked
"""

FEATURES = """
## Features

- `serde`: `Serialize` and `Deserialize` for `Record`.
"""

USAGE = """
## Usage

```rust
let record = logfmt::parse(r#"level=warn msg="disk almost full""#)?;
assert_eq!(record.get("level"), Some("warn"));
println!("{record}");
```
"""

BRANCHES = (
    Branch(
        "readme-usage",
        "merged",
        Turn(
            "Add a short usage example to the README.",
            "Added a Usage section with a four-line example that parses a line, reads one field "
            "and prints the record back.",
            tools=(("read", "Read src/lib.rs"), ("edit", "Edit README.md")),
            files={"README.md": README + USAGE},
        ),
    ),
    Branch(
        "ci-msrv",
        "merged",
        Turn(
            "Set the minimum supported Rust version to 1.74 and test it in CI.",
            "Cargo.toml now declares rust-version 1.74, and the CI workflow runs the tests on 1.74 "
            "and on stable. Nothing in the crate needed a newer compiler.",
            tools=(("edit", "Edit Cargo.toml"), ("edit", "Write .github/workflows/ci.yml")),
            files={
                "Cargo.toml": CARGO_TOML.replace('edition = "2021"\n', 'edition = "2021"\nrust-version = "1.74"\n'),
                ".github/workflows/ci.yml": CI_YML,
            },
        ),
    ),
    Branch(
        "escapes",
        "interrupted",
        Turn(
            'Support escaped quotes inside quoted values, like msg="say \\"hi\\"".',
            None,
            say="The value scanner stops at the first quote, so escapes need a small state machine. "
            "Reading the parser first.",
            tools=(("read", "Read src/parse.rs"), ("execute", "Run cargo test")),
        ),
    ),
    Branch(
        "bench-parse",
        "running",
        Turn(
            "Add criterion benchmarks for parse() on a short line and on a 64 KB log.",
            None,
            say="Adding criterion as a dev-dependency and a benches/parse.rs with two groups.",
            tools=(
                ("edit", "Edit Cargo.toml"),
                ("edit", "Write benches/parse.rs"),
                ("execute", "Run cargo bench --no-run"),
            ),
        ),
    ),
    Branch(
        "fuzz-parser",
        "running",
        Turn(
            "Add a cargo-fuzz target for parse() and fix any panic it finds.",
            None,
            say="Setting up fuzz/fuzz_targets/parse.rs, then a five-minute run.",
            tools=(("execute", "Run cargo fuzz init"), ("execute", "Run cargo fuzz run parse -- -max_total_time=300")),
        ),
    ),
    Branch(
        "serde-feature",
        "ready",
        Turn(
            "Put serde support for Record behind an optional serde feature.",
            "Record derives Serialize and Deserialize when the serde feature is on. The feature is "
            "off by default, so the crate still has no dependencies, and the README says how to "
            "turn it on.",
            tools=(
                ("edit", "Edit Cargo.toml"),
                ("edit", "Edit src/record.rs"),
                ("edit", "Edit README.md"),
                ("execute", "Run cargo test --all-features"),
            ),
            files={
                "README.md": README + USAGE + FEATURES,
                "Cargo.toml": CARGO_TOML.replace('edition = "2021"\n', 'edition = "2021"\nrust-version = "1.74"\n')
                + 'serde = { version = "1", features = ["derive"], optional = true }\n',
                "src/record.rs": (RECORD_HEAD + RECORD_TAIL).replace(
                    "#[derive(Debug, Default, Clone, PartialEq)]\n",
                    "#[derive(Debug, Default, Clone, PartialEq)]\n"
                    '#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]\n',
                ),
            },
        ),
    ),
    Branch(
        OPEN_BRANCH,
        "ready",
        Turn(
            "Make the flaky parser test deterministic. Add a regression test.",
            "prints_a_line_back failed in 7 of 20 runs. Record kept its fields in a HashMap, and "
            "Display printed them in hash order, which changes from run to run.\n\n"
            "Record now keeps its fields in a Vec, in the order they appear in the line. Setting a "
            "key that is already there replaces its value in place.\n\n"
            "Two new tests: keeps_field_order_on_every_run prints a five-field line back 100 times, "
            "and a_repeated_key_keeps_its_place covers the replace case. The whole suite passed 20 "
            "runs in a row.",
            say="I'll reproduce the failure first, then look at how Record stores its fields.",
            tools=(
                ("execute", "Run cargo test --test parser, 20 times"),
                ("read", "Read src/record.rs"),
                ("edit", "Edit src/record.rs"),
                ("edit", "Edit tests/parser.rs"),
                ("execute", "Run cargo test, 20 times"),
            ),
            files={
                "src/record.rs": RECORD_FIXED_HEAD + RECORD_TAIL,
                "tests/parser.rs": TESTS_HEAD + TESTS_ADDED + TESTS_TAIL,
            },
        ),
    ),
)

PROJECT = "Retention"
WORKSPACE = "Q3 churn review"
OPEN_SESSION = "Churn analysis"
OPEN_OUTPUT = ("churn_by_plan.csv", 2)
"""The output the screenshot previews: its name, and the turn of the open session that wrote it."""

# account, plan, region, MRR at the start of the quarter, MRR at its end
ACCOUNTS = (
    ("Juniper Bakery", "Starter", "North America", 99, 0),
    ("Pinecrest Dental", "Starter", "North America", 99, 99),
    ("Atlas Tutoring", "Starter", "Europe", 49, 49),
    ("Harbor Yoga", "Starter", "Asia Pacific", 99, 99),
    ("Quill & Co", "Starter", "Europe", 149, 149),
    ("Maple Street Books", "Starter", "North America", 49, 49),
    ("Birchwood Florist", "Starter", "Europe", 99, 0),
    ("Lumen Studio", "Starter", "Asia Pacific", 149, 149),
    ("Fernhill Farm", "Starter", "North America", 49, 49),
    ("Copperleaf Cafe", "Starter", "Asia Pacific", 49, 0),
    ("Driftwood Surf School", "Starter", "Asia Pacific", 99, 99),
    ("Oakline Carpentry", "Starter", "North America", 149, 149),
    ("Northwind Freight", "Team", "North America", 490, 0),
    ("Bluebird Analytics", "Team", "Europe", 390, 390),
    ("Orchard Health", "Team", "North America", 490, 490),
    ("Tidewater Legal", "Team", "Europe", 290, 0),
    ("Kestrel Robotics", "Team", "Asia Pacific", 490, 490),
    ("Saffron Kitchens", "Team", "Asia Pacific", 390, 390),
    ("Granite Peak Realty", "Team", "North America", 390, 390),
    ("Wavelength Media", "Team", "Europe", 490, 490),
    ("Cobalt Security", "Team", "North America", 290, 290),
    ("Riverbend Clinic", "Team", "North America", 390, 390),
    ("Helix Biotech", "Business", "Europe", 1900, 1900),
    ("Summit Logistics", "Business", "North America", 2400, 2400),
    ("Marigold Retail", "Business", "Asia Pacific", 1200, 1200),
    ("Ironclad Insurance", "Business", "North America", 2400, 2400),
    ("Evergreen Schools", "Business", "Europe", 1900, 1500),
    ("Polaris Energy", "Enterprise", "North America", 9500, 9500),
    ("Meridian Bank", "Enterprise", "Europe", 12000, 12000),
    ("Sakura Telecom", "Enterprise", "Asia Pacific", 8000, 8000),
)

# A survey reason, and how a sentence says it.
REASONS = {"Price": "over price", "Competitor": "to competitors", "Closed down": "to closures"}

# account, reason, comment
SURVEY = (
    ("Juniper Bakery", "Price", "We only used invoicing, and the rest wasn't worth it."),
    ("Birchwood Florist", "Price", "Card fees went up, so we cut subscriptions."),
    ("Copperleaf Cafe", "Closed down", "We closed the cafe in August."),
    ("Northwind Freight", "Competitor", "Switched to a tool our carriers already use."),
    ("Tidewater Legal", "Competitor", "Found a plan with document storage included."),
)

PLANS = ("Starter", "Team", "Business", "Enterprise")
NUMBERS = ("No", "One", "Two", "Three", "Four", "Five")


def _csv(header: tuple[str, ...], rows: Iterable[tuple[Any, ...]]) -> str:
    out = io.StringIO()
    writer = csv.writer(out, lineterminator="\n")
    writer.writerow(header)
    writer.writerows(rows)
    return out.getvalue()


SOURCES = {
    "accounts_q3.csv": _csv(("account", "plan", "region", "mrr_start", "mrr_end"), ACCOUNTS),
    "cancellation_survey.csv": _csv(("account", "reason", "comment"), SURVEY),
}


@dataclass(frozen=True)
class Plan:
    """One plan's Q3: its revenue at the start, what it lost, and its top cancellation reason."""

    name: str
    start: int
    lost: int
    reason: str | None

    @property
    def percent(self) -> float:
        return round(100 * self.lost / self.start, 1)


def plans() -> list[Plan]:
    out = []
    for name in PLANS:
        rows = [a for a in ACCOUNTS if a[1] == name]
        accounts = {a[0] for a in rows}
        reasons = Counter(reason for account, reason, _ in SURVEY if account in accounts)
        out.append(
            Plan(
                name,
                sum(a[3] for a in rows),
                sum(a[3] - a[4] for a in rows),
                reasons.most_common(1)[0][0] if reasons else None,
            )
        )
    return out


def _summary(text: str) -> str:
    return "```json\n" + json.dumps({"summary": text}) + "\n```"


def _and(items: list[str]) -> str:
    return ", ".join(items[:-1]) + " and " + items[-1] if len(items) > 1 else "".join(items)


def _session_turns() -> tuple[Turn, ...]:
    every = plans()
    lost = sum(p.lost for p in every)
    start = sum(p.start for p in every)
    flagged = [p for p in every if p.percent > 10]
    rest = [p for p in every if p.percent <= 10]
    first = Turn(
        "Work out how much monthly revenue we lost in Q3, by plan.",
        f"We lost ${lost:,} of ${start:,} in monthly revenue in Q3, {100 * lost / start:.1f}%. "
        "churn_by_plan.csv has a row for each plan.\n\n" + _summary(f"Q3 lost ${lost:,} of ${start:,} MRR."),
        outputs={
            "churn_by_plan.csv": _csv(
                ("Plan", "MRR lost", "Lost %"),
                ((p.name, p.lost, p.percent) for p in every),
            )
        },
    )
    second = Turn(
        "Add the top cancellation reason for each plan, and tell me which plans lost more than 10%.",
        "I added a Top reason column from the cancellation survey.\n\n"
        f"{_and([p.name for p in flagged])} lost more than 10% of their revenue:\n"
        + "".join(f"- {p.name}: {p.percent}%, mostly {REASONS[p.reason or '']}\n" for p in flagged)
        + _and([f"{p.name} lost {f'{p.percent}%' if p.lost else 'nothing'}" for p in rest])
        + ".\n\n"
        + _summary(f"{_and([p.name for p in flagged])} lost more than 10% of MRR in Q3."),
        outputs={
            "churn_by_plan.csv": _csv(
                ("Plan", "MRR lost", "Lost %", "Top reason"),
                ((p.name, p.lost, p.percent, p.reason or "") for p in every),
            )
        },
    )
    regions = {a[0]: (a[2], a[3]) for a in ACCOUNTS}
    starter = [
        (account, reason, comment)
        for account, reason, comment in SURVEY
        if account in {a[0] for a in ACCOUNTS if a[1] == "Starter"}
    ]
    price = sum(1 for _, reason, _ in starter if reason == "Price")
    third = Turn(
        "Which Starter accounts cancelled, and what did they say?",
        f"{NUMBERS[len(starter)]} Starter accounts cancelled:\n"
        + "".join(
            f'- {account}, ${regions[account][1]}, {reason.lower()}: "{comment}"\n'
            for account, reason, comment in starter
        )
        + "starter_cancellations.csv has them with their regions.\n\n"
        + _summary(f"{NUMBERS[len(starter)]} Starter accounts cancelled, {NUMBERS[price].lower()} over price."),
        outputs={
            "starter_cancellations.csv": _csv(
                ("Account", "Region", "MRR", "Reason", "Comment"),
                ((account, *regions[account], reason, comment) for account, reason, comment in starter),
            )
        },
    )
    return first, second, third


def _themes() -> str:
    counts = Counter(reason for _, reason, _ in SURVEY)
    listed = "".join(f"- {reason}: {n}\n" for reason, n in counts.most_common())
    return f"{NUMBERS[len(SURVEY)]} cancelled accounts left a comment. By theme:\n\n{listed}"


SESSIONS = (
    Session(OPEN_SESSION, True, _session_turns()),
    Session(
        "Survey themes",
        False,
        (Turn("Group the cancellation comments into themes and count each one.", _themes()),),
    ),
)
