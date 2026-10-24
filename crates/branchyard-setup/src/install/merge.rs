//! Merging a packaged fragment into a harness's own settings file, by the
//! rules every `merge-*` mode shares: objects (tables, mappings) merge key
//! by key, deeply; a list gains the fragment's entries it does not already
//! hold (equal by value); nothing is ever removed or replaced; a key whose
//! value differs in kind or, for a scalar, in value is refused with a
//! message naming it. Merging the same fragment twice changes nothing.
//!
//! `at` is a dotted key the fragment merges under (`""`: the top level).

use serde_json::{Map, Value};
use toml_edit::{DocumentMut, Item, Table};

/// The dotted path `at` as keys; `""` is none.
fn keys(at: &str) -> Vec<&str> {
    at.split('.').filter(|k| !k.is_empty()).collect()
}

fn path_of(prefix: &str, key: &str) -> String {
    match prefix {
        "" => key.to_owned(),
        p => format!("{p}.{key}"),
    }
}

// ---- JSON -----------------------------------------------------------------

/// Merge the JSON `fragment` into `existing` (`None`: no file yet) under
/// `at`. Returns the new text, or `existing` byte for byte when the merge
/// changes nothing. Keeps the file's key order and its indentation.
pub fn json(existing: Option<&str>, fragment: &str, at: &str) -> Result<String, String> {
    let fragment: Value = serde_json::from_str(fragment)
        .map_err(|e| format!("the packaged fragment is not JSON: {e}"))?;
    let mut wrapped = fragment;
    for key in keys(at).into_iter().rev() {
        let mut map = Map::new();
        map.insert(key.to_owned(), wrapped);
        wrapped = Value::Object(map);
    }
    let (mut target, indent) = match existing {
        None => (Value::Object(Map::new()), "  ".to_owned()),
        Some(text) if text.trim().is_empty() => (Value::Object(Map::new()), "  ".to_owned()),
        Some(text) => (
            serde_json::from_str(text).map_err(|e| {
                format!("not plain JSON ({e}); comments and trailing commas are not merged: add the fragment by hand")
            })?,
            json_indent(text),
        ),
    };
    let before = target.clone();
    merge_json_value(&mut target, &wrapped, "")?;
    if let Some(text) = existing {
        if target == before {
            return Ok(text.to_owned());
        }
    }
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    serde::Serialize::serialize(&target, &mut serializer).map_err(|e| e.to_string())?;
    let mut text = String::from_utf8(out).map_err(|e| e.to_string())?;
    text.push('\n');
    Ok(text)
}

/// The indentation of the first indented line: spaces or a tab.
fn json_indent(text: &str) -> String {
    text.lines()
        .find_map(|line| {
            let indent: String = line
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect();
            (!indent.is_empty() && indent.len() < line.len()).then_some(indent)
        })
        .unwrap_or_else(|| "  ".to_owned())
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

fn merge_json_value(target: &mut Value, fragment: &Value, path: &str) -> Result<(), String> {
    match (target, fragment) {
        (Value::Object(target), Value::Object(fragment)) => {
            for (key, value) in fragment {
                match target.get_mut(key) {
                    None => {
                        target.insert(key.clone(), value.clone());
                    }
                    Some(existing) => merge_json_value(existing, value, &path_of(path, key))?,
                }
            }
            Ok(())
        }
        (Value::Array(target), Value::Array(fragment)) => {
            for value in fragment {
                if !target.contains(value) {
                    target.push(value.clone());
                }
            }
            Ok(())
        }
        (target, fragment) if target == fragment => Ok(()),
        (target, fragment) => Err(clash(path, kind(target), kind(fragment))),
    }
}

/// Why a key is refused: it holds another kind of value, or (the same
/// kind, a scalar) another value.
fn clash(path: &str, has: &str, wants: &str) -> String {
    let at = match path {
        "" => "the top level".to_owned(),
        p => format!("`{p}`"),
    };
    match has == wants {
        true => format!("{at} is already set to a different value; Branchyard does not replace it: change it by hand or remove it, then run again"),
        false => format!("{at} is {has} here but Branchyard's fragment has {wants} there; nothing is replaced: merge it by hand"),
    }
}

// ---- TOML -----------------------------------------------------------------

/// Merge the TOML `fragment` into `existing` under `at`, with `toml_edit`
/// so the file's comments, order and formatting stay as they are.
pub fn toml(existing: Option<&str>, fragment: &str, at: &str) -> Result<String, String> {
    let fragment: DocumentMut = fragment
        .parse()
        .map_err(|e| format!("the packaged fragment is not TOML: {e}"))?;
    let mut doc: DocumentMut = existing
        .unwrap_or("")
        .parse()
        .map_err(|e| format!("not valid TOML: {e}"))?;
    let before = doc.to_string();
    let mut target: &mut Table = doc.as_table_mut();
    let mut path = String::new();
    for key in keys(at) {
        path = path_of(&path, key);
        if !target.contains_key(key) {
            let mut table = Table::new();
            table.set_implicit(true);
            target.insert(key, Item::Table(table));
        }
        target = match target.get_mut(key).and_then(Item::as_table_mut) {
            Some(table) => table,
            None => return Err(clash(&path, "not a table", "a table")),
        };
    }
    merge_toml_table(target, fragment.as_table(), &path)?;
    let after = doc.to_string();
    match existing {
        Some(text) if after == before => Ok(text.to_owned()),
        _ => Ok(after),
    }
}

/// A TOML item's value, for comparing two items by value whatever their
/// formatting or form (inline or standard table).
fn toml_value(item: &Item) -> Option<toml::Value> {
    let mut doc = DocumentMut::new();
    doc.insert("v", item.clone());
    let table: toml::Table = toml::from_str(&doc.to_string()).ok()?;
    table.get("v").cloned()
}

fn toml_kind(item: &Item) -> &'static str {
    match item {
        Item::None => "nothing",
        Item::Table(_) => "a table",
        Item::ArrayOfTables(_) => "an array of tables",
        Item::Value(v) if v.is_inline_table() => "a table",
        Item::Value(v) if v.is_array() => "a list",
        Item::Value(_) => "a value",
    }
}

fn merge_toml_table(
    target: &mut dyn toml_edit::TableLike,
    fragment: &dyn toml_edit::TableLike,
    path: &str,
) -> Result<(), String> {
    for (key, value) in fragment.iter() {
        let here = path_of(path, key);
        let Some(existing) = target.get_mut(key) else {
            target.insert(key, value.clone());
            continue;
        };
        if let (Some(t), Some(f)) = (existing.as_table_like_mut(), value.as_table_like()) {
            merge_toml_table(t, f, &here)?;
            continue;
        }
        match (&mut *existing, value) {
            (Item::ArrayOfTables(t), Item::ArrayOfTables(f)) => {
                for table in f.iter() {
                    let wanted = toml_value(&Item::Table(table.clone()));
                    let present = t
                        .iter()
                        .any(|have| toml_value(&Item::Table(have.clone())) == wanted);
                    if !present {
                        t.push(table.clone());
                    }
                }
            }
            (Item::Value(toml_edit::Value::Array(t)), Item::Value(toml_edit::Value::Array(f))) => {
                for v in f.iter() {
                    let wanted = toml_value(&Item::Value(v.clone()));
                    let present = t
                        .iter()
                        .any(|have| toml_value(&Item::Value(have.clone())) == wanted);
                    if !present {
                        let mut v = v.clone();
                        v.decor_mut().clear();
                        t.push_formatted(v);
                    }
                }
            }
            (existing, value) => {
                if toml_value(existing) != toml_value(value) {
                    return Err(clash(&here, toml_kind(existing), toml_kind(value)));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "by hook session-start"}]}]}}"#;

    #[test]
    fn json_merges_deeply_appends_unique_and_is_idempotent() {
        let user = "{\n    \"model\": \"opus\",\n    \"hooks\": {\n        \"SessionStart\": [\n            {\"hooks\": [{\"type\": \"command\", \"command\": \"mine\"}]}\n        ]\n    }\n}\n";
        let once = json(Some(user), CLAUDE, "").unwrap();
        let value: Value = serde_json::from_str(&once).unwrap();
        assert_eq!(value["model"], "opus");
        let starts = value["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(starts.len(), 2, "{once}");
        assert_eq!(starts[0]["hooks"][0]["command"], "mine");
        // Key order and the four-space indentation are kept.
        assert!(once.starts_with("{\n    \"model\""), "{once}");
        let twice = json(Some(&once), CLAUDE, "").unwrap();
        assert_eq!(twice, once);
        // A new file is the fragment.
        let fresh = json(None, CLAUDE, "").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&fresh).unwrap(),
            serde_json::from_str::<Value>(CLAUDE).unwrap()
        );
    }

    #[test]
    fn json_refuses_clashes_and_comments() {
        let e = json(Some(r#"{"hooks": []}"#), CLAUDE, "").unwrap_err();
        assert!(e.contains("`hooks` is a list"), "{e}");
        let e = json(Some(r#"{"a": {"b": 1}}"#), r#"{"b": 2}"#, "a").unwrap_err();
        assert!(e.contains("`a.b` is already set"), "{e}");
        let e = json(Some("{ // mine\n}"), CLAUDE, "").unwrap_err();
        assert!(e.contains("not plain JSON"), "{e}");
        // `at` nests the fragment.
        let at = json(Some("{}"), r#"{"x": 1}"#, "a.b").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&at).unwrap(),
            serde_json::json!({"a": {"b": {"x": 1}}})
        );
    }

    #[test]
    fn toml_keeps_comments_and_formatting_and_is_idempotent() {
        let user = "# my codex settings\nmodel = \"o3\"  # the best\n\n[features]\nfoo = true\nlist = [\"a\"]\n";
        let fragment = "[features]\nhooks = true\nlist = [\"a\", \"b\"]\n\n[[hooks.SessionStart]]\ncommand = \"by hook --harness codex session-start\"\n";
        let once = toml(Some(user), fragment, "").unwrap();
        assert!(
            once.starts_with("# my codex settings\nmodel = \"o3\"  # the best\n"),
            "{once}"
        );
        let parsed: toml::Table = toml::from_str(&once).unwrap();
        assert_eq!(parsed["features"]["hooks"].as_bool(), Some(true));
        assert_eq!(parsed["features"]["list"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        let twice = toml(Some(&once), fragment, "").unwrap();
        assert_eq!(twice, once);
        let e = toml(Some("[features]\nhooks = false\n"), fragment, "").unwrap_err();
        assert!(e.contains("`features.hooks` is already set"), "{e}");
        let e = toml(Some("features = 1\n"), fragment, "").unwrap_err();
        assert!(e.contains("`features` is a value"), "{e}");
        let at = toml(None, "x = 1\n", "a.b").unwrap();
        assert_eq!(
            toml::from_str::<toml::Table>(&at).unwrap()["a"]["b"]["x"].as_integer(),
            Some(1)
        );
    }
}
