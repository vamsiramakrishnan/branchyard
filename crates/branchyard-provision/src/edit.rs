// Partly derived from Scion (https://github.com/GoogleCloudPlatform/scion)
// at d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338: the dotted-path merge of
// _walk_dotted_path and _merge_into_file, and read_json_skipping_comment_lines
// with its test, in harnesses/scion_harness.py and
// harnesses/scion_harness_test.py. Copyright 2026 Google LLC. Licensed under
// the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to Rust as pure edits of
// a file's content; unparseable JSON is refused instead of replaced by an
// empty object; unchanged documents are returned byte for byte; dotenv
// merging and array appends are new.

//! File edits as data, and their pure application to a file's current
//! content.
//!
//! Each edit changes what it names and keeps the rest: JSON keys are set or
//! removed by path, TOML top-level keys and tables are reconciled line by
//! line as Scion's `scion_harness.py` does (without a TOML library), an
//! instructions block is kept between markers. A file that cannot be parsed
//! is refused, never overwritten. Errors name the problem, never the
//! content.

use serde_json::{Map, Value};

use crate::{instructions, toml};

/// One change to one file.
#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    /// Replace the whole content.
    Put(String),
    /// Remove the file if it exists.
    Remove,
    /// Merge into a JSON object, created if the file is missing.
    Json {
        edits: Vec<JsonEdit>,
        /// Ignore lines starting with `//` when reading, as GitHub Copilot
        /// CLI's `config.json` allows. They are not written back.
        comment_lines: bool,
    },
    /// Reconcile a TOML document.
    Toml(Vec<TomlEdit>),
    /// Set Branchyard's instructions block, keeping everything outside it;
    /// `None` removes the block, and the file if nothing else is left.
    Block(Option<String>),
    /// Set `NAME=value` lines of a dotenv file, keeping other lines.
    Dotenv(Vec<(String, String)>),
    /// Remove the lines of a dotenv file that define these names, and the
    /// file if nothing but blank lines is left.
    DotenvUnset(Vec<String>),
}

/// A change to a JSON object at a key path.
#[derive(Clone, Debug, PartialEq)]
pub enum JsonEdit {
    /// Set the value, creating intermediate objects.
    Set(Vec<String>, Value),
    /// Set the value only if the key is absent.
    Default(Vec<String>, Value),
    /// Append to the array there (created if absent) unless present.
    Push(Vec<String>, Value),
    /// Remove the key if present.
    Remove(Vec<String>),
    /// Remove the key if it holds exactly this value.
    RemoveIf(Vec<String>, Value),
}

/// A change to a TOML document's top level.
#[derive(Clone, Debug, PartialEq)]
pub enum TomlEdit {
    /// Remove every top-level assignment of the key.
    RemoveKey(String),
    /// Set a top-level key to a TOML literal, placed before the first table
    /// so it stays at the top level.
    SetKey { key: String, literal: String },
    /// Remove `[name]` and every `[name.…]` table.
    RemoveTables(String),
    /// Append a table's text at the end.
    AppendTable(String),
}

/// A JSON key path from string slices.
pub fn path(keys: &[&str]) -> Vec<String> {
    keys.iter().map(|k| (*k).to_owned()).collect()
}

impl Edit {
    /// Merge `other` into this edit where both are the same kind; returns
    /// `other` back otherwise.
    pub(crate) fn absorb(&mut self, other: Edit) -> Option<Edit> {
        match (self, other) {
            (
                Edit::Json { edits, .. },
                Edit::Json {
                    edits: more,
                    comment_lines: false,
                },
            ) => {
                edits.extend(more);
                None
            }
            (Edit::Toml(edits), Edit::Toml(more)) => {
                edits.extend(more);
                None
            }
            (Edit::Dotenv(pairs), Edit::Dotenv(more)) => {
                pairs.extend(more);
                None
            }
            (_, other) => Some(other),
        }
    }

    /// The new content given the current one (`None`: no file). `None`
    /// back removes the file. An unchanged document returns the current
    /// content exactly, so applying twice changes nothing.
    pub fn apply(&self, current: Option<&str>) -> Result<Option<String>, String> {
        match self {
            Edit::Put(text) => Ok(Some(text.clone())),
            Edit::Remove => Ok(None),
            // Edits that leave a missing file empty do not create it.
            Edit::Json {
                edits,
                comment_lines,
            } => json(current, edits, *comment_lines)
                .map(|text| (current.is_some() || text != "{}\n").then_some(text)),
            Edit::Toml(edits) => Ok(Some(toml_document(current.unwrap_or(""), edits))),
            Edit::Block(text) => instructions::merge_block(current, text.as_deref()),
            Edit::Dotenv(pairs) => dotenv(current.unwrap_or(""), pairs).map(Some),
            Edit::DotenvUnset(names) => Ok(current.and_then(|text| dotenv_unset(text, names))),
        }
    }
}

/// Apply `edits` in order to `current`.
pub fn apply_all(edits: &[Edit], current: Option<&str>) -> Result<Option<String>, String> {
    let mut content = current.map(str::to_owned);
    for edit in edits {
        content = edit.apply(content.as_deref())?;
    }
    Ok(content)
}

/// `value` with every object's keys in sorted order, whichever map type
/// `serde_json` was built with.
pub(crate) fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k.clone(), sorted(v)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// Parse a JSON object, ignoring `//` lines when asked (Scion's
/// `read_json_skipping_comment_lines`). Empty text is an empty object.
pub fn parse_object(text: &str, comment_lines: bool) -> Result<Map<String, Value>, String> {
    let text: String = match comment_lines {
        true => text
            .lines()
            .filter(|line| !line.trim().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n"),
        false => text.to_owned(),
    };
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err("is not a JSON object; not overwriting it".into()),
        Err(error) => Err(format!(
            "is not valid JSON (line {}, column {}); not overwriting it",
            error.line(),
            error.column()
        )),
    }
}

#[allow(clippy::expect_used)] // ratchet: branchyard-provision
fn json(current: Option<&str>, edits: &[JsonEdit], comment_lines: bool) -> Result<String, String> {
    let before = parse_object(current.unwrap_or(""), comment_lines)?;
    let mut root = Value::Object(before.clone());
    for edit in edits {
        match edit {
            JsonEdit::Set(path, value) => *slot(&mut root, path) = value.clone(),
            JsonEdit::Default(path, value) => {
                let slot = slot(&mut root, path);
                if slot.is_null() {
                    *slot = value.clone();
                }
            }
            JsonEdit::Push(path, value) => {
                let slot = slot(&mut root, path);
                if !slot.is_array() {
                    *slot = Value::Array(Vec::new());
                }
                let items = slot.as_array_mut().expect("made an array");
                if !items.contains(value) {
                    items.push(value.clone());
                }
            }
            JsonEdit::Remove(path) => remove(&mut root, path),
            JsonEdit::RemoveIf(path, value) => {
                if lookup(&root, path) == Some(value) {
                    remove(&mut root, path);
                }
            }
        }
    }
    match (current, &root) {
        (Some(text), Value::Object(after)) if *after == before => Ok(text.to_owned()),
        _ => Ok(crate::json_text(&root)),
    }
}

/// The value at `path`, creating objects on the way; a non-object on the
/// way is replaced, as Scion's `_walk_dotted_path` does.
#[allow(clippy::expect_used)] // ratchet: branchyard-provision
fn slot<'a>(root: &'a mut Value, path: &[String]) -> &'a mut Value {
    let mut current = root;
    for key in path {
        if !current.is_object() {
            *current = Value::Object(Map::new());
        }
        current = current
            .as_object_mut()
            .expect("made an object")
            .entry(key.clone())
            .or_insert(Value::Null);
    }
    current
}

fn lookup<'a>(root: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter()
        .try_fold(root, |current, key| current.get(key.as_str()))
}

fn remove(root: &mut Value, path: &[String]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut current = root;
    for key in parents {
        match current.get_mut(key.as_str()) {
            Some(next) => current = next,
            None => return,
        }
    }
    if let Some(map) = current.as_object_mut() {
        map.remove(last);
    }
}

fn toml_document(current: &str, edits: &[TomlEdit]) -> String {
    let mut content = current.to_owned();
    for edit in edits {
        content = match edit {
            TomlEdit::RemoveKey(key) => toml::strip_top_level_key(&content, key),
            TomlEdit::SetKey { key, literal } => {
                let stripped = toml::strip_top_level_key(&content, key);
                toml::set_top_level(&stripped, key, literal)
            }
            TomlEdit::RemoveTables(name) => {
                let exact = format!("[{name}]");
                let prefix = format!("[{name}.");
                toml::strip_sections(&content, |header| {
                    header == exact || header.starts_with(&prefix)
                })
            }
            TomlEdit::AppendTable(text) => toml::merge_document(&content, text),
        };
    }
    let normalized = format!("{}\n", content.trim());
    // A document whose edits changed nothing but blank lines is left as is.
    if current.trim() == normalized.trim() && !current.is_empty() {
        return current.to_owned();
    }
    normalized
}

/// Whether a dotenv line defines `name`, with or without `export`.
fn defines(line: &str, name: &str) -> bool {
    let line = line.trim_start();
    let line = line.strip_prefix("export ").unwrap_or(line);
    line.strip_prefix(name)
        .is_some_and(|rest| rest.trim_start().starts_with('='))
}

fn dotenv_unset(current: &str, names: &[String]) -> Option<String> {
    let kept: Vec<&str> = current
        .lines()
        .filter(|line| !names.iter().any(|name| defines(line, name)))
        .collect();
    if kept.iter().all(|line| line.trim().is_empty()) {
        return None;
    }
    let mut text = kept.join("\n");
    text.push('\n');
    match text == current {
        true => Some(current.to_owned()),
        false => Some(text),
    }
}

fn dotenv(current: &str, pairs: &[(String, String)]) -> Result<String, String> {
    let mut lines: Vec<String> = current.lines().map(str::to_owned).collect();
    for (name, value) in pairs {
        if value.contains(['\n', '\r']) {
            return Err(format!("the value for {name} spans lines"));
        }
        let line = format!("{name}={value}");
        let defines = |l: &String| defines(l, name);
        match lines.iter().position(defines) {
            Some(index) => {
                lines[index] = line;
                lines.retain({
                    let mut seen = 0;
                    move |l| {
                        if defines(l) {
                            seen += 1;
                            seen == 1
                        } else {
                            true
                        }
                    }
                });
            }
            None => lines.push(line),
        }
    }
    let mut text = lines.join("\n");
    text.push('\n');
    if text == current {
        return Ok(current.to_owned());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_edits_merge_and_keep_other_keys() {
        let current = r#"{"user": {"theme": "dark"}, "keep": [1]}"#;
        let edit = Edit::Json {
            edits: vec![
                JsonEdit::Set(path(&["security", "auth", "selectedType"]), json!("x")),
                JsonEdit::Default(path(&["user", "theme"]), json!("light")),
                JsonEdit::Push(path(&["keep"]), json!(2)),
                JsonEdit::Push(path(&["keep"]), json!(1)),
            ],
            comment_lines: false,
        };
        let out: Value =
            serde_json::from_str(&edit.apply(Some(current)).unwrap().unwrap()).unwrap();
        assert_eq!(
            out,
            json!({"user": {"theme": "dark"}, "keep": [1, 2],
                   "security": {"auth": {"selectedType": "x"}}})
        );
    }

    #[test]
    fn unchanged_json_is_returned_byte_for_byte() {
        let current = "{\n    \"a\": 1\n}\n";
        let edit = Edit::Json {
            edits: vec![JsonEdit::Set(path(&["a"]), json!(1))],
            comment_lines: false,
        };
        assert_eq!(edit.apply(Some(current)).unwrap().unwrap(), current);
    }

    #[test]
    fn unparseable_json_is_refused_without_quoting_it() {
        let edit = Edit::Json {
            edits: vec![JsonEdit::Set(path(&["a"]), json!(1))],
            comment_lines: false,
        };
        let error = edit.apply(Some("{\"token\": sk-secret")).unwrap_err();
        assert!(error.contains("not valid JSON") && !error.contains("sk-secret"));
        assert!(edit
            .apply(Some("[1]"))
            .unwrap_err()
            .contains("not a JSON object"));
    }

    #[test]
    fn comment_lines_are_skipped_when_reading() {
        // From Scion's TestReadJsonSkippingComments.
        let map = parse_object("// comment\n{\"key\": \"value\"}\n// another", true).unwrap();
        assert_eq!(map["key"], "value");
        assert!(parse_object("// c\n{}", false).is_err());
    }

    #[test]
    fn toml_keys_go_to_the_top_level_even_after_tables() {
        let current = "model = \"a\"\n\n[projects.\"/workspace\"]\ntrust_level = \"trusted\"\n";
        let out = Edit::Toml(vec![TomlEdit::SetKey {
            key: "model_reasoning_effort".into(),
            literal: "\"high\"".into(),
        }])
        .apply(Some(current))
        .unwrap()
        .unwrap();
        assert_eq!(
            out,
            "model = \"a\"\nmodel_reasoning_effort = \"high\"\n\n\
             [projects.\"/workspace\"]\ntrust_level = \"trusted\"\n"
        );
    }

    #[test]
    fn dotenv_lines_are_unset_and_an_empty_file_removed() {
        let unset = |current: &str, names: &[&str]| {
            Edit::DotenvUnset(names.iter().map(|n| (*n).to_owned()).collect())
                .apply(Some(current))
                .unwrap()
        };
        assert_eq!(
            unset("# mine\nA=1\nexport B=2\nC=3\n", &["A", "B"]).as_deref(),
            Some("# mine\nC=3\n")
        );
        assert_eq!(unset("A=1\n\n", &["A"]), None);
        assert_eq!(unset("C=3\n", &["A"]).as_deref(), Some("C=3\n"));
        assert_eq!(
            Edit::DotenvUnset(vec!["A".into()]).apply(None).unwrap(),
            None
        );
    }

    #[test]
    fn dotenv_lines_are_set_in_place() {
        let out = Edit::Dotenv(vec![("A".into(), "2".into()), ("C".into(), "3".into())])
            .apply(Some("# keep\nexport A=1\nB=x\nA=old\n"))
            .unwrap()
            .unwrap();
        assert_eq!(out, "# keep\nA=2\nB=x\nC=3\n");
        assert!(Edit::Dotenv(vec![("A".into(), "x\ny".into())])
            .apply(None)
            .is_err());
    }
}
