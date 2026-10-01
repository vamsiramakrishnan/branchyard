// Derived from stablyai/orca src/shared/orca-yaml.ts and
// src/shared/orca-yaml-hook-types.ts, at revision
// 280733273545f0b3eeedc1be54b14d406239030e.
// Copyright (c) 2026 Lovecast Inc. Licensed under the MIT License; the
// license text, which must accompany substantial portions of this code, is
// in vendor/orca/LICENSE.
// Modified for Branchyard: translated from TypeScript to Rust, keeping the
// parts a worktree's lifecycle uses: `scripts.setup` and `scripts.archive`
// (trimmed, empty treated as absent, as `asTrimmedString` does),
// `defaultTabs` (normalized as `normalizeDefaultTabs` does, colors
// dropped), and `worktree.sharedDirectories` (normalized as
// `normalizeSharedDirectories` does, reported rather than imported).
// Orca's environment recipes, issue command and agent startup policy are
// left out. Orca parses with the `yaml` package; this reads the block
// subset of YAML such files use (mappings, sequences, plain and quoted
// scalars, `|` and `>` block scalars) and refuses anything else, such as
// flow collections, anchors and tags. The result is mapped to
// `[workspace]`, which Orca does not do.

//! `orca.yaml`.

use serde_json::{json, Map, Value};

use super::{rename_variables, script_commands, Imported, Reader};

pub const FILE: &str = "orca.yaml";

/// Orca's limits on one file's work: entries beyond them are ignored.
const MAX_SHARED_DIRECTORIES: usize = 100;
const MAX_COLLECTION_ENTRIES: usize = 100;

/// Orca's hook variables and Branchyard's equivalents.
const VARIABLES: &[(&str, &str)] = &[
    ("ORCA_ROOT_PATH", "BRANCHYARD_ROOT"),
    ("ORCA_WORKTREE_PATH", "BRANCHYARD_WORKTREE"),
];

/// The lifecycle parts of Orca's `OrcaHooks`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OrcaHooks {
    /// Runs after a worktree is created.
    pub setup: Option<String>,
    /// Runs before a worktree is archived.
    pub archive: Option<String>,
    /// Terminal tabs a new worktree opens: `(title, command)`.
    pub default_tabs: Vec<(Option<String>, Option<String>)>,
    /// Directories linked, not copied, into each worktree.
    pub shared_directories: Vec<String>,
}

/// `asTrimmedString`.
fn trimmed(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// `normalizeSharedDirectories`: repository-relative paths, deduplicated,
/// without absolute paths, `..`, `.`, empty segments or `.git`.
fn shared_directories(value: Option<&Value>) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for entry in value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_SHARED_DIRECTORIES)
    {
        let Some(raw) = trimmed(Some(entry)) else {
            continue;
        };
        let normalized = raw.replace('\\', "/");
        let normalized = normalized.strip_prefix("./").unwrap_or(&normalized);
        let normalized = normalized.trim_end_matches('/');
        let segments: Vec<&str> = normalized.split('/').collect();
        let drive = normalized.len() >= 2
            && normalized.as_bytes()[1] == b':'
            && normalized.as_bytes()[0].is_ascii_alphabetic();
        if normalized.is_empty()
            || normalized.starts_with('/')
            || drive
            || segments
                .iter()
                .any(|s| matches!(*s, ".." | "." | "" | ".git"))
        {
            continue;
        }
        if !seen.iter().any(|s| s == normalized) {
            seen.push(normalized.to_owned());
        }
    }
    seen
}

/// `normalizeDefaultTabs`, without colors.
fn default_tabs(value: Option<&Value>) -> Vec<(Option<String>, Option<String>)> {
    let Some(items) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    if items.len() > MAX_COLLECTION_ENTRIES {
        return Vec::new();
    }
    items
        .iter()
        .filter_map(|entry| {
            let record = entry.as_object()?;
            let title = trimmed(record.get("title"));
            let command = trimmed(record.get("command"));
            (title.is_some() || command.is_some()).then_some((title, command))
        })
        .collect()
}

/// `parseOrcaYaml`'s lifecycle parts; `None` when the file sets none of
/// them, an error when it is not YAML this reads.
pub fn parse(text: &str) -> Result<Option<OrcaHooks>, String> {
    let root = yaml(text)?;
    let Some(record) = root.as_object() else {
        return Ok(None);
    };
    let scripts = record.get("scripts").and_then(Value::as_object);
    let hooks = OrcaHooks {
        setup: scripts.and_then(|s| trimmed(s.get("setup"))),
        archive: scripts.and_then(|s| trimmed(s.get("archive"))),
        default_tabs: default_tabs(record.get("defaultTabs")),
        shared_directories: shared_directories(
            record
                .get("worktree")
                .and_then(Value::as_object)
                .and_then(|w| w.get("sharedDirectories")),
        ),
    };
    Ok((hooks != OrcaHooks::default()).then_some(hooks))
}

pub fn import(text: &str, _: Reader) -> Result<Imported, String> {
    let Some(hooks) = parse(text)? else {
        return Ok(Imported {
            file: FILE,
            ..Imported::default()
        });
    };
    let commands = |script: &Option<String>| -> Vec<String> {
        script
            .as_deref()
            .map(script_commands)
            .unwrap_or_default()
            .into_iter()
            .map(|c| rename_variables(&c, VARIABLES))
            .collect()
    };
    let mut notes = Vec::new();
    let mut tabs = hooks.default_tabs.iter().filter(|(_, c)| c.is_some());
    let run = tabs.next().and_then(|(_, c)| c.clone());
    let rest: Vec<String> = tabs
        .map(|(title, command)| title.clone().or(command.clone()).unwrap_or_default())
        .collect();
    if !rest.is_empty() {
        notes.push(format!(
            "only the first defaultTabs command became the run script; not imported: {}",
            rest.join(", ")
        ));
    }
    if !hooks.shared_directories.is_empty() {
        notes.push(format!(
            "worktree.sharedDirectories ({}) is not imported: it links directories into every \
             worktree, which [workspace] copy does not do",
            hooks.shared_directories.join(", ")
        ));
    }
    Ok(Imported {
        file: FILE,
        copy: Vec::new(),
        setup: commands(&hooks.setup),
        run: run.map(|c| rename_variables(&c, VARIABLES)),
        teardown: commands(&hooks.archive),
        notes,
    })
}

// The YAML subset.

struct Line<'a> {
    number: usize,
    indent: usize,
    text: &'a str,
}

/// A YAML document of block mappings, block sequences, scalars and block
/// scalars, as JSON; anything else is refused with its line.
pub fn yaml(text: &str) -> Result<Value, String> {
    let lines: Vec<Line> = text
        .lines()
        .enumerate()
        .map(|(i, raw)| Line {
            number: i + 1,
            indent: raw.len() - raw.trim_start_matches(' ').len(),
            text: raw.trim_start_matches(' '),
        })
        .collect();
    if text.contains('\t') && lines.iter().any(|l| l.text.starts_with('\t')) {
        return Err("tabs indent a line; YAML indents with spaces".into());
    }
    let mut at = 0;
    skip_blank(&lines, &mut at);
    if lines.get(at).is_some_and(|l| l.text.trim_end() == "---") {
        at += 1;
        skip_blank(&lines, &mut at);
    }
    let Some(first) = lines.get(at) else {
        return Ok(Value::Null);
    };
    let value = block(&lines, &mut at, first.indent)?;
    skip_blank(&lines, &mut at);
    if let Some(line) = lines.get(at) {
        return Err(format!("line {}: unexpected indentation", line.number));
    }
    Ok(value)
}

fn is_blank(line: &Line) -> bool {
    line.text.trim().is_empty() || line.text.starts_with('#')
}

fn skip_blank(lines: &[Line], at: &mut usize) {
    while lines.get(*at).is_some_and(is_blank) {
        *at += 1;
    }
}

/// A mapping or sequence whose entries start at `indent`.
fn block(lines: &[Line], at: &mut usize, indent: usize) -> Result<Value, String> {
    skip_blank(lines, at);
    let Some(first) = lines.get(*at) else {
        return Ok(Value::Null);
    };
    if first.text == "-" || first.text.starts_with("- ") {
        sequence(lines, at, indent)
    } else {
        mapping(lines, at, indent)
    }
}

fn sequence(lines: &[Line], at: &mut usize, indent: usize) -> Result<Value, String> {
    let mut items = Vec::new();
    loop {
        skip_blank(lines, at);
        let Some(line) = lines.get(*at) else { break };
        let dash = line.text == "-" || line.text.starts_with("- ");
        // A key at the sequence's indentation ends it (`key:\n- a\nnext:`).
        if line.indent < indent || (line.indent == indent && !dash) {
            break;
        }
        if line.indent > indent {
            return Err(format!("line {}: expected a sequence entry", line.number));
        }
        let rest = line.text[1..].trim_start();
        let inner = indent + (line.text.len() - rest.len());
        if rest.is_empty() || rest.starts_with('#') {
            *at += 1;
            items.push(nested(lines, at, indent)?);
        } else if key_of(rest).is_some() {
            // `- key: value` starts a mapping indented to `key`.
            items.push(mapping_from(lines, at, inner, Some(rest))?);
        } else {
            let number = line.number;
            *at += 1;
            items.push(scalar_or_block(rest, lines, at, indent, number)?);
        }
    }
    Ok(Value::Array(items))
}

fn mapping(lines: &[Line], at: &mut usize, indent: usize) -> Result<Value, String> {
    mapping_from(lines, at, indent, None)
}

/// A mapping at `indent`; `first` is its first entry's text when it sits
/// after a sequence's `- `.
fn mapping_from(
    lines: &[Line],
    at: &mut usize,
    indent: usize,
    mut first: Option<&str>,
) -> Result<Value, String> {
    let mut map = Map::new();
    loop {
        let (text, number) = match first.take() {
            Some(text) => (text, lines[*at].number),
            None => {
                skip_blank(lines, at);
                let Some(line) = lines.get(*at) else { break };
                if line.indent < indent {
                    break;
                }
                if line.indent > indent {
                    return Err(format!("line {}: unexpected indentation", line.number));
                }
                (line.text, line.number)
            }
        };
        let Some((key, rest)) = key_of(text) else {
            return Err(format!("line {number}: expected `key: value`"));
        };
        if map.contains_key(&key) {
            return Err(format!("line {number}: {key} is given twice"));
        }
        *at += 1;
        let rest = rest.trim();
        let value = if rest.is_empty() || rest.starts_with('#') {
            nested(lines, at, indent)?
        } else {
            scalar_or_block(rest, lines, at, indent, number)?
        };
        map.insert(key, value);
    }
    Ok(Value::Object(map))
}

/// The value indented under a key or `-` at `indent`, or null. A sequence
/// may sit at the key's own indentation, as YAML allows.
fn nested(lines: &[Line], at: &mut usize, indent: usize) -> Result<Value, String> {
    skip_blank(lines, at);
    match lines.get(*at) {
        Some(line) if line.indent > indent => block(lines, at, line.indent),
        Some(line)
            if line.indent == indent && (line.text == "-" || line.text.starts_with("- ")) =>
        {
            sequence(lines, at, indent)
        }
        _ => Ok(Value::Null),
    }
}

/// `key: rest` with a plain or quoted key.
fn key_of(text: &str) -> Option<(String, &str)> {
    if let Some(quote) = text.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let end = text[1..].find(quote)? + 1;
        let rest = text[end + 1..].strip_prefix(':')?;
        if !(rest.is_empty() || rest.starts_with(' ')) {
            return None;
        }
        return Some((text[1..end].to_owned(), rest));
    }
    let colon = text
        .find(": ")
        .or_else(|| text.strip_suffix(':').map(|t| t.len()))?;
    let key = text[..colon].trim();
    if key.is_empty() || key.starts_with(['#', '[', '{', '&', '*', '!', '|', '>']) {
        return None;
    }
    Some((key.to_owned(), &text[colon + 1..]))
}

fn scalar_or_block(
    text: &str,
    lines: &[Line],
    at: &mut usize,
    indent: usize,
    number: usize,
) -> Result<Value, String> {
    let text = strip_comment(text);
    if let Some(header) = text.strip_prefix('|').or_else(|| text.strip_prefix('>')) {
        let folded = text.starts_with('>');
        let chomp = header.trim();
        if !matches!(chomp, "" | "-" | "+") {
            return Err(format!(
                "line {number}: block scalar indicator {text:?} is not supported"
            ));
        }
        return Ok(Value::String(block_scalar(
            lines, at, indent, folded, chomp,
        )));
    }
    scalar(text).map_err(|e| format!("line {number}: {e}"))
}

/// The text of a `|` or `>` block scalar under `indent`.
fn block_scalar(
    lines: &[Line],
    at: &mut usize,
    indent: usize,
    folded: bool,
    chomp: &str,
) -> String {
    let mut body: Vec<String> = Vec::new();
    let mut inner = None;
    while let Some(line) = lines.get(*at) {
        if line.text.trim().is_empty() {
            body.push(String::new());
            *at += 1;
            continue;
        }
        if line.indent <= indent {
            break;
        }
        let inner = *inner.get_or_insert(line.indent);
        if line.indent < inner {
            break;
        }
        body.push(format!("{}{}", " ".repeat(line.indent - inner), line.text));
        *at += 1;
    }
    let mut text = match folded {
        true => body.join(" "),
        false => body.join("\n"),
    };
    match chomp {
        "-" => text.truncate(text.trim_end_matches('\n').len()),
        "+" => text.push('\n'),
        _ => {
            text.truncate(text.trim_end_matches('\n').len());
            text.push('\n');
        }
    }
    text
}

/// A plain scalar's text up to a ` #` comment; quoted scalars keep `#`.
fn strip_comment(text: &str) -> &str {
    let text = text.trim();
    if text.starts_with(['"', '\'']) {
        return text;
    }
    match text.find(" #") {
        Some(at) => text[..at].trim_end(),
        None => text,
    }
}

fn scalar(text: &str) -> Result<Value, String> {
    if let Some(inner) = text.strip_prefix('\'') {
        let end = inner.rfind('\'').ok_or("unterminated quote")?;
        return Ok(Value::String(inner[..end].replace("''", "'")));
    }
    if text.starts_with('"') {
        let (value, _) = serde_json_string(text)?;
        return Ok(Value::String(value));
    }
    if text.starts_with(['[', '{', '&', '*', '!', '%', '@', '`']) {
        return Err(format!("{text:?} uses YAML this reader does not support"));
    }
    Ok(match text {
        "" | "~" | "null" | "Null" | "NULL" => Value::Null,
        "true" | "True" | "TRUE" => Value::Bool(true),
        "false" | "False" | "FALSE" => Value::Bool(false),
        _ => match text.parse::<i64>() {
            Ok(n) => json!(n),
            Err(_) => Value::String(text.to_owned()),
        },
    })
}

/// A double-quoted scalar, whose escapes are JSON's for what files use.
fn serde_json_string(text: &str) -> Result<(String, usize), String> {
    let mut escaped = false;
    for (i, c) in text.char_indices().skip(1) {
        match (escaped, c) {
            (true, _) => escaped = false,
            (false, '\\') => escaped = true,
            (false, '"') => {
                let value: String =
                    serde_json::from_str(&text[..=i]).map_err(|e| format!("bad string: {e}"))?;
                return Ok((value, i + 1));
            }
            _ => {}
        }
    }
    Err("unterminated quote".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn the_block_subset_reads_as_json() {
        let value = yaml(
            "---\n# comment\nscripts:\n  setup: |\n    node a.mjs\n\n    pnpm install\n  archive: \"echo 'bye'\" \n\
             list:\n- a\n- 'b c' # note\n- n: 1\n  m: true\nempty:\nfold: >-\n  one\n  two\n",
        )
        .unwrap();
        assert_eq!(
            value,
            json!({
                "scripts": {"setup": "node a.mjs\n\npnpm install\n", "archive": "echo 'bye'"},
                "list": ["a", "b c", {"n": 1, "m": true}],
                "empty": null,
                "fold": "one two"
            })
        );
        for bad in [
            "a: [1, 2]",
            "a: &x 1",
            "a: 1\na: 2",
            "a:\n    b: 1\n  c: 2",
            "- a\nb: 1",
        ] {
            assert!(yaml(bad).is_err(), "{bad:?} was accepted");
        }
        assert_eq!(yaml("").unwrap(), Value::Null);
    }

    #[test]
    fn orca_yaml_maps_to_a_workspace() {
        let imported = import(
            "scripts:\n  setup: |\n    node config/scripts/setup.mjs\n    cp \"$ORCA_ROOT_PATH/.env\" .env\n    pnpm install\n  archive: docker compose down\n\
             defaultTabs:\n  - title: Dev\n    color: '#ff0000'\n    command: pnpm dev\n  - title: Tests\n    command: pnpm test --watch\n  - title: Shell\n\
             worktree:\n  sharedDirectories:\n    - ./node_modules/\n    - ../escape\n    - .git\n    - node_modules\n",
            &none,
        )
        .unwrap();
        assert_eq!(
            imported.setup,
            [
                "node config/scripts/setup.mjs",
                "cp \"$BRANCHYARD_ROOT/.env\" .env",
                "pnpm install"
            ]
        );
        assert_eq!(imported.teardown, ["docker compose down"]);
        assert_eq!(imported.run.as_deref(), Some("pnpm dev"));
        assert_eq!(imported.notes.len(), 2, "{:?}", imported.notes);
        assert!(imported.notes[0].ends_with("not imported: Tests"));
        assert!(imported.notes[1].contains("(node_modules)"));
        assert_eq!(parse("scripts:\n  setup: '   '\n").unwrap(), None);
        assert!(import("scripts: {setup: x}", &none).is_err());
    }

    /// The vendored source still reads the fields this port reads, the
    /// way it reads them.
    #[test]
    fn the_port_follows_the_vendored_parser() {
        let source = include_str!("../../../../vendor/orca/src/shared/orca-yaml.ts");
        for needle in [
            "const setup = scriptsRecord ? asTrimmedString(scriptsRecord.setup) : undefined",
            "const archive = scriptsRecord ? asTrimmedString(scriptsRecord.archive) : undefined",
            "const defaultTabs = normalizeDefaultTabs(record.defaultTabs)",
            "normalizeSharedDirectories(worktreeRecord.sharedDirectories)",
            "const MAX_SHARED_DIRECTORIES = 100",
            "segments.includes('.git')",
        ] {
            assert!(
                source.contains(needle),
                "{needle} is gone from orca-yaml.ts"
            );
        }
    }
}
