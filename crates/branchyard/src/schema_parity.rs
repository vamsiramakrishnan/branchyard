//! SQLite and PostgreSQL keep the same state in two hand-written schemas
//! (`sqlite.rs` `SCHEMA`, `pg.rs` `TABLES` and `STEPS`). Nothing else
//! ties them together, so a column added to one and forgotten in the other
//! shows up as a runtime failure on whichever backend a user did not test.
//! This test reads both from source, in every build (the PostgreSQL
//! backend is a feature, its text is not), and compares tables and
//! columns after stripping what is legitimately different: the `by_`
//! prefix, the type spelling, the `repo` column PostgreSQL scopes rows
//! with, and the differences listed in [`ALLOWED`], each with its reason.
//!
//! Adding a table or column? Add it to both schemas. A difference that is
//! real goes in [`ALLOWED`] with the reason; the test fails on an entry
//! that no longer matches anything, so the list cannot go stale.

use std::collections::{BTreeMap, BTreeSet};

const SQLITE: &str = include_str!("sqlite.rs");
const POSTGRES: &str = include_str!("pg.rs");

/// What is deliberately different, as `"table"`, `"table.column"` (a column
/// in only one backend, written `sqlite:` or `pg:`) or
/// `"table.column:type"` (a column spelled with a different type class).
const ALLOWED: &[(&str, &str)] = &[(
    "pg:feed_heads",
    "PostgreSQL numbers one feed per repository from a counter row; SQLite's \
         `events.id AUTOINCREMENT` is the feed's order.",
)];

/// A column, as `(type class, nullable)`.
type Column = (String, bool);
type Schema = BTreeMap<String, BTreeMap<String, Column>>;

/// The type class of an SQL type, so `INTEGER` and `BIGINT` compare equal.
/// SQLite has no boolean type: it stores `0`/`1` as an integer, so a
/// PostgreSQL `BOOLEAN` is the same class.
fn class(sql_type: &str) -> String {
    match sql_type.to_ascii_uppercase().as_str() {
        "INTEGER" | "BIGINT" | "INT" | "SMALLINT" | "BOOLEAN" | "BOOL" => "int",
        "TEXT" | "VARCHAR" => "text",
        "REAL" | "DOUBLE" | "FLOAT" => "real",
        "BLOB" | "BYTEA" => "blob",
        other => return other.to_ascii_lowercase(),
    }
    .to_owned()
}

/// The text between the `(` at `open` and its matching `)`.
fn block(text: &str, open: usize) -> &str {
    let mut depth = 0;
    for (i, c) in text[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &text[open + 1..open + i];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced parentheses after {:?}", &text[open..open + 40]);
}

/// Items of a column list, split on the commas outside parentheses.
fn items(list: &str) -> Vec<String> {
    let (mut out, mut depth, mut current) = (Vec::new(), 0, String::new());
    for c in list.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    out.push(current);
    out.into_iter()
        .map(|i| i.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|i| !i.is_empty())
        .collect()
}

/// A column definition: `name TYPE ...`, or `None` for a table constraint.
fn column(definition: &str) -> Option<(String, Column)> {
    let mut words = definition.split_whitespace();
    let name = words.next()?;
    if ["PRIMARY", "UNIQUE", "FOREIGN", "CHECK", "CONSTRAINT"].contains(&name) {
        return None;
    }
    let kind = words.next()?;
    let upper = definition.to_ascii_uppercase();
    let nullable = !upper.contains("NOT NULL") && !upper.contains("PRIMARY KEY");
    // `DOUBLE PRECISION` is one type, `GENERATED ... AS IDENTITY` and
    // `AUTOINCREMENT` are how a key counts, not part of the type.
    Some((name.to_owned(), (class(kind), nullable)))
}

/// Every table the text creates, with the columns it later adds.
fn schema(text: &str, prefix: &str) -> Schema {
    // Drop SQL and Rust line comments, and the line-continuation
    // backslashes of string literals, so the DDL reads as plain SQL.
    let text: String = text
        .lines()
        .map(|l| {
            let l = l.split("--").next().unwrap_or("");
            let l = l.trim_start();
            if l.starts_with("//") {
                ""
            } else {
                l.trim_end_matches('\\')
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut tables = Schema::new();
    for marker in ["CREATE TABLE IF NOT EXISTS "] {
        let mut from = 0;
        while let Some(at) = text[from..].find(marker) {
            let start = from + at + marker.len();
            let open = start + text[start..].find('(').expect("a column list");
            let name = text[start..open].trim();
            from = open;
            if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue; // `CREATE TABLE IF NOT EXISTS {p}name`, in a comment or format string
            }
            let name = name.strip_prefix(prefix).unwrap_or(name).to_owned();
            let columns = items(block(&text, open))
                .iter()
                .filter_map(|i| column(i))
                .collect();
            tables.insert(name, columns);
        }
    }
    let mut from = 0;
    while let Some(at) = text[from..].find("ALTER TABLE ") {
        let start = from + at + "ALTER TABLE ".len();
        let end = start + text[start..].find('"').unwrap_or(text.len() - start);
        from = start;
        let statement = text[start..end]
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let Some((table, rest)) = statement.split_once(" ADD COLUMN ") else {
            continue;
        };
        let rest = rest.strip_prefix("IF NOT EXISTS ").unwrap_or(rest);
        let table = table.strip_prefix(prefix).unwrap_or(table);
        if let (Some(columns), Some((name, column))) = (tables.get_mut(table), column(rest)) {
            columns.insert(name, column);
        }
    }
    tables
}

/// What differs between the two schemas, one line each, as [`ALLOWED`]
/// spells them.
fn differences(sqlite: &Schema, pg: &Schema) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for table in sqlite.keys().chain(pg.keys()).collect::<BTreeSet<_>>() {
        let (a, b) = (sqlite.get(table), pg.get(table));
        let (Some(a), Some(b)) = (a, b) else {
            out.insert(format!(
                "{}:{table}",
                if a.is_some() { "sqlite" } else { "pg" }
            ));
            continue;
        };
        for name in a.keys().chain(b.keys()).collect::<BTreeSet<_>>() {
            // PostgreSQL scopes every shared table's rows to a repository.
            if name == "repo" && b.contains_key("repo") && !a.contains_key("repo") {
                continue;
            }
            match (a.get(name), b.get(name)) {
                (Some(_), None) => {
                    out.insert(format!("sqlite:{table}.{name}"));
                }
                (None, Some(_)) => {
                    out.insert(format!("pg:{table}.{name}"));
                }
                (Some(x), Some(y)) if x != y => {
                    out.insert(format!("{table}.{name}:{} vs {}", x.0, y.0));
                }
                _ => {}
            }
        }
    }
    out
}

fn compare(sqlite: &str, pg: &str) -> BTreeSet<String> {
    differences(&schema(sqlite, ""), &schema(pg, "by_"))
}

#[test]
fn both_backends_define_the_same_tables_and_columns() {
    let found = compare(SQLITE, POSTGRES);
    let allowed: BTreeSet<String> = ALLOWED.iter().map(|(d, _)| (*d).to_owned()).collect();
    let unexplained: Vec<_> = found.difference(&allowed).collect();
    let stale: Vec<_> = allowed.difference(&found).collect();
    assert!(
        unexplained.is_empty(),
        "the SQLite and PostgreSQL schemas differ; add it to the other backend or, if it is \
         real, to ALLOWED in schema_parity.rs with the reason:\n{}",
        unexplained
            .iter()
            .map(|d| format!("  {d}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        stale.is_empty(),
        "ALLOWED lists differences that no longer exist; remove them: {stale:?}"
    );
}

#[test]
fn the_parser_sees_both_schemas() {
    let (sqlite, pg) = (schema(SQLITE, ""), schema(POSTGRES, "by_"));
    // A parser that found nothing would make the comparison vacuous.
    for (name, tables) in [("sqlite", &sqlite), ("pg", &pg)] {
        assert!(tables.len() > 25, "{name}: {} tables", tables.len());
        let leases = &tables["leases"];
        assert_eq!(leases["pid"], ("int".to_owned(), false), "{name}");
        // Columns added by `ALTER TABLE` count.
        assert!(
            tables["branches"].contains_key("parent_incarnation"),
            "{name}"
        );
        assert!(
            tables["artifacts"].contains_key("ancestry_incarnations"),
            "{name}"
        );
    }
}

#[test]
fn a_column_added_to_one_backend_is_reported() {
    // The guard itself: break a copy of SQLite's schema and expect the
    // difference.
    let broken = SQLITE.replacen(
        "    created_ms INTEGER NOT NULL,\n    answered INTEGER NOT NULL,",
        "    created_ms INTEGER NOT NULL,\n    answered INTEGER NOT NULL,\n    extra TEXT,",
        1,
    );
    assert_ne!(broken, SQLITE, "the fixture must match the schema text");
    let found = compare(&broken, POSTGRES);
    assert!(found.contains("sqlite:approval_asks.extra"), "{found:?}");
    let renamed = POSTGRES.replacen(
        "at_ms BIGINT NOT NULL,\n    activity",
        "when_ms BIGINT NOT NULL,\n    activity",
        1,
    );
    assert_ne!(renamed, POSTGRES);
    let found = compare(SQLITE, &renamed);
    assert!(
        found.contains("sqlite:events.at_ms") && found.contains("pg:events.when_ms"),
        "{found:?}"
    );
}
