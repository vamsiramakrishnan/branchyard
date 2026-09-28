// Originally derived from Scion (https://github.com/GoogleCloudPlatform/scion)
// at d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338: the TOML helpers of
// harnesses/scion_harness.py (toml_escape, toml_inline_table,
// toml_string_array, strip_toml_sections) and _is_toml_key_line and
// _strip_toml_top_level_key of harnesses/codex/provision.py. Tests from
// harnesses/scion_harness_test.py and harnesses/codex/provision_test.py.
// Copyright 2026 Google LLC. Licensed under the Apache License, Version
// 2.0, for the parts credited to it above.
//
// Rewritten for Branchyard on `toml_edit` (a real TOML parser and editor,
// so it keeps the original translation's guarantee more reliably:
// existing comments, formatting and dotted-key structure it does not
// touch are round-tripped rather than approximated by scanning lines).
// The document surgery below (`strip_sections`, `strip_top_level_key`,
// `set_top_level`, `merge_document`) is Branchyard's own on top of it;
// `escape`, `string`, `inline_table` and `string_array`, which build
// small literal fragments rather than edit a document, are unchanged
// from Scion's originals. See `patches/scion-provision.json`'s
// `rewritten` entry for this file: unlike a `derivatives` entry, no
// longer tracked line-for-line against a vendored blob, since it is no
// longer a line-for-line port.

//! TOML document editing on `toml_edit`.
//!
//! These functions parse the whole document to make a change, but
//! preserve everything they do not touch (comments, formatting, table
//! order) since `toml_edit::DocumentMut` is a lossless editor, not a
//! plain deserializer.

use toml_edit::{DocumentMut, Table};

/// Escape a string for a TOML basic string.
pub fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// A TOML basic string literal.
pub fn string(value: &str) -> String {
    format!("\"{}\"", escape(value))
}

/// A TOML inline table with quoted keys and values, sorted by key.
pub fn inline_table<'a>(items: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let mut items: Vec<_> = items.into_iter().collect();
    items.sort();
    let parts: Vec<String> = items
        .iter()
        .map(|(k, v)| format!("\"{}\" = \"{}\"", escape(k), escape(v)))
        .collect();
    format!("{{ {} }}", parts.join(", "))
}

/// A TOML array of strings.
pub fn string_array<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    let parts: Vec<String> = items.into_iter().map(string).collect();
    format!("[{}]", parts.join(", "))
}

/// Parse `content` as a TOML document; an unparsable document (which the
/// caller elsewhere refuses before ever reaching here) is treated as
/// empty rather than panicking, keeping these functions total.
fn parse(content: &str) -> DocumentMut {
    content.parse().unwrap_or_default()
}

/// Remove the tables whose bracketed header (its dotted key path
/// re-bracketed, e.g. `[otel]` or `[mcp_servers.foo]`) satisfies
/// `matches`, checked at every depth. Removing a table removes everything
/// nested under it, so a match on `[otel]` also drops
/// `[otel.exporter."otlp-grpc"]`.
pub fn strip_sections(content: &str, matches: impl Fn(&str) -> bool) -> String {
    let mut doc = parse(content);
    strip_matching(doc.as_table_mut(), "", &matches);
    doc.to_string()
}

fn strip_matching(table: &mut Table, prefix: &str, matches: &impl Fn(&str) -> bool) {
    let keys: Vec<String> = table.iter().map(|(k, _)| k.to_owned()).collect();
    for key in keys {
        let header = match prefix.is_empty() {
            true => format!("[{key}]"),
            false => format!("[{prefix}.{key}]"),
        };
        if matches(&header) {
            table.remove(&key);
            continue;
        }
        let path = match prefix.is_empty() {
            true => key.clone(),
            false => format!("{prefix}.{key}"),
        };
        let Some(item) = table.get_mut(&key) else {
            continue;
        };
        if let Some(sub) = item.as_table_mut() {
            strip_matching(sub, &path, matches);
        } else if let Some(array) = item.as_array_of_tables_mut() {
            for sub in array.iter_mut() {
                strip_matching(sub, &path, matches);
            }
        }
    }
}

/// Whether `line` assigns exactly `key` (used only by callers that still
/// scan text themselves, not by this module).
pub fn is_key_line(line: &str, key: &str) -> bool {
    let s = line.trim();
    s.strip_prefix(key)
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| matches!(c, ' ' | '=' | '\t'))
}

/// Remove every top-level assignment of `key`; assignments of the same
/// name inside a table stay, since only the document's own, headerless
/// table is touched.
pub fn strip_top_level_key(content: &str, key: &str) -> String {
    let mut doc = parse(content);
    doc.as_table_mut().remove(key);
    doc.to_string()
}

/// Set a top-level key to a raw TOML value literal (as built by
/// [`string`] or [`inline_table`], or any other valid TOML value text).
/// It renders before the first table regardless of insertion order:
/// `toml_edit` always prints a table's own scalar assignments before any
/// table nested under it.
pub fn set_top_level(content: &str, key: &str, literal: &str) -> String {
    let mut doc = parse(content);
    let value: toml_edit::Value = literal
        .parse()
        .unwrap_or_else(|_| toml_edit::Value::from(literal));
    doc.as_table_mut()[key] = toml_edit::Item::Value(value);
    doc.to_string()
}

/// Insert a top-level `line` (`"key = literal"`) after the last top-level
/// line, before the first table. Kept for its existing callers and tests;
/// prefer [`set_top_level`] in new code, which needs no line to parse.
pub fn insert_top_level(content: &str, line: &str) -> String {
    let (key, literal) = line
        .split_once('=')
        .unwrap_or_else(|| panic!("{line:?} is not \"key = literal\""));
    set_top_level(content, key.trim(), literal.trim())
}

/// Merge `text` (a small, valid TOML document, typically one `[section]`
/// with whatever it nests) into `content` at the top level, replacing any
/// of the same top-level keys `text` itself defines.
pub fn merge_document(content: &str, text: &str) -> String {
    let mut doc = parse(content);
    let fragment = parse(text);
    for (key, item) in fragment.as_table().iter() {
        let mut item = item.clone();
        // `item` carries the header's position and formatting as parsed
        // from the small, standalone `text` document (so, typically,
        // "the very first table, no blank line before it"): reset both,
        // so it instead renders where a table newly added to `content`
        // normally would, after whatever precedes it there, with the
        // usual leading blank line.
        reset_table_position(&mut item);
        doc.as_table_mut().insert(key, item);
    }
    doc.to_string()
}

fn reset_table_position(item: &mut toml_edit::Item) {
    if let Some(table) = item.as_table_mut() {
        *table.decor_mut() = toml_edit::Decor::default();
        table.set_position(None);
        for (_, sub) in table.iter_mut() {
            reset_table_position(sub);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // From Scion's scion_harness_test.py.

    #[test]
    fn basic_escapes() {
        assert_eq!(escape("hello"), "hello");
        assert_eq!(escape("a\"b"), "a\\\"b");
        assert_eq!(escape("a\\b"), "a\\\\b");
        assert_eq!(escape("a\nb"), "a\\nb");
        assert_eq!(escape("a\rb"), "a\\rb");
        assert_eq!(escape("a\tb"), "a\\tb");
    }

    #[test]
    fn inline_tables_sort_their_keys() {
        assert_eq!(
            inline_table([("b", "2"), ("a", "1")]),
            "{ \"a\" = \"1\", \"b\" = \"2\" }"
        );
    }

    #[test]
    fn string_arrays() {
        assert_eq!(string_array(["x", "y"]), "[\"x\", \"y\"]");
    }

    #[test]
    fn strip_otel() {
        let content = "key = 1\n\n[otel]\nenabled = true\n\n[other]\nval = 2\n";
        let result = strip_sections(content, |h| h == "[otel]");
        assert!(!result.contains("[otel]"));
        assert!(!result.contains("enabled = true"));
        assert!(result.contains("[other]") && result.contains("val = 2"));
    }

    #[test]
    fn strip_mcp_sections() {
        let content =
            "base = 1\n\n[mcp_servers.foo]\ncommand = \"x\"\n\n[mcp_servers.bar]\nurl = \"y\"\n";
        let result = strip_sections(content, |h| h.starts_with("[mcp_servers."));
        assert!(!result.contains("[mcp_servers."));
        assert!(result.contains("base = 1"));
    }

    #[test]
    fn strip_nested_otel_tables_too() {
        let content = "[otel]\nenabled = true\n\n[otel.exporter.\"otlp-grpc\"]\nendpoint = \"x\"\n";
        let result = strip_sections(content, |h| h == "[otel]");
        assert!(!result.contains("otel"));
    }

    #[test]
    fn no_match_unchanged() {
        let content = "[regular]\nkey = \"val\"\n";
        assert_eq!(strip_sections(content, |h| h == "[nonexistent]"), content);
    }

    // From Scion's codex/provision_test.py.

    #[test]
    fn strip_top_level_key_section_safety() {
        let content = "[otel]\nreasoning_effort = \"low\"\n[other]\nkey = \"val\"\n";
        let result = strip_top_level_key(content, "reasoning_effort");
        assert!(result.contains("reasoning_effort = \"low\""));
    }

    #[test]
    fn strip_top_level_key_does_not_match_prefixed_keys() {
        let content = "reasoning_effort = \"low\"\nreasoning_effort_extended = \"yes\"\n";
        let result = strip_top_level_key(content, "reasoning_effort");
        assert!(!result.contains("reasoning_effort = \"low\""));
        assert!(result.contains("reasoning_effort_extended = \"yes\""));
    }

    #[test]
    fn top_level_lines_go_before_the_first_table() {
        assert_eq!(insert_top_level("", "a = 1"), "a = 1\n");
        assert_eq!(insert_top_level("x = 1\n", "a = 1"), "x = 1\na = 1\n");
        // `toml_edit` keeps a parsed table's own captured formatting
        // (here, no blank line before `[t]`, since the source had none)
        // rather than inserting one; a table `merge_document` creates
        // fresh still gets one, by `toml_edit`'s own default. Either way
        // the top-level key renders before the table, which is what this
        // is really checking.
        assert_eq!(
            insert_top_level("[t]\nk = 1\n", "a = 1"),
            "a = 1\n[t]\nk = 1\n"
        );
    }

    #[test]
    fn merging_a_document_replaces_its_own_top_level_keys() {
        let content = "keep = 1\n\n[otel]\nold = true\n";
        let merged = merge_document(content, "[otel]\nnew = true\n");
        assert!(merged.contains("keep = 1"));
        assert!(merged.contains("[otel]"));
        assert!(merged.contains("new = true"));
        assert!(!merged.contains("old"));
    }
}
