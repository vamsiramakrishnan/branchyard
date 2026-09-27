// Derived from Scion (https://github.com/GoogleCloudPlatform/scion) at
// d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338: the TOML helpers of
// harnesses/scion_harness.py (toml_escape, toml_inline_table,
// toml_string_array, strip_toml_sections) and _is_toml_key_line and
// _strip_toml_top_level_key of harnesses/codex/provision.py.
// Tests from harnesses/scion_harness_test.py and
// harnesses/codex/provision_test.py. Copyright 2026 Google LLC. Licensed
// under the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to Rust; the predicate is
// a closure over the trimmed header; insert_top_level is new, because
// Scion appends `model_reasoning_effort` after the last table, where TOML
// reads it as a key of that table.

//! Line-based TOML editing, as Scion does it without a TOML library.
//!
//! These functions edit text; they never parse a whole document, so
//! comments and formatting outside what they touch are kept.

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

fn is_header(trimmed: &str) -> bool {
    trimmed.starts_with('[') && trimmed.ends_with(']')
}

/// Remove the tables whose trimmed header line `matches`, with the blank
/// lines just before each.
pub fn strip_sections(content: &str, matches: impl Fn(&str) -> bool) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let mut keep = vec![true; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        let stripped = lines[i].trim();
        if is_header(stripped) && matches(stripped) {
            let end = (i + 1..lines.len())
                .find(|&j| is_header(lines[j].trim()))
                .unwrap_or(lines.len());
            let mut start = i;
            while start > 0 && lines[start - 1].trim().is_empty() && keep[start - 1] {
                start -= 1;
            }
            keep[start..end].iter_mut().for_each(|k| *k = false);
            i = end;
        } else {
            i += 1;
        }
    }
    lines
        .iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(line, _)| *line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether `line` assigns exactly `key`.
pub fn is_key_line(line: &str, key: &str) -> bool {
    let s = line.trim();
    s.strip_prefix(key)
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| matches!(c, ' ' | '=' | '\t'))
}

/// Remove top-level assignments of `key`; assignments inside tables stay.
pub fn strip_top_level_key(content: &str, key: &str) -> String {
    let mut in_section = false;
    content
        .split('\n')
        .filter(|line| {
            if line.trim().starts_with('[') {
                in_section = true;
            }
            in_section || !is_key_line(line, key)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Insert a top-level `line` after the last top-level line, before the
/// first table.
pub fn insert_top_level(content: &str, line: &str) -> String {
    let mut lines: Vec<&str> = content.split('\n').collect();
    let first_table = lines
        .iter()
        .position(|l| is_header(l.trim()))
        .unwrap_or(lines.len());
    let mut at = first_table;
    while at > 0 && lines[at - 1].trim().is_empty() {
        at -= 1;
    }
    lines.insert(at, line);
    // Keep a blank line between the top level and the first table.
    if at + 1 < lines.len() && is_header(lines[at + 1].trim()) {
        lines.insert(at + 1, "");
    }
    lines.join("\n")
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
        let content = "base = 1\n\n[mcp_servers.foo]\ncommand = x\n\n[mcp_servers.bar]\nurl = y\n";
        let result = strip_sections(content, |h| h.starts_with("[mcp_servers."));
        assert!(!result.contains("[mcp_servers."));
        assert!(result.contains("base = 1"));
    }

    #[test]
    fn no_match_unchanged() {
        let content = "[regular]\nkey = val\n";
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
        assert_eq!(
            insert_top_level("[t]\nk = 1\n", "a = 1"),
            "a = 1\n\n[t]\nk = 1\n"
        );
    }
}
