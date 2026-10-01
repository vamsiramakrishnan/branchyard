//! A map's items, its prompt template, its branches' answers and its
//! result table, as text: no branch is run here. See `docs/map.md`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::Error;

/// How a list of items is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemFormat {
    /// One JSON value per line.
    Jsonl,
    /// One JSON array.
    Json,
    /// Comma-separated values with a header row; each row is an object of
    /// strings keyed by the header.
    Csv,
    /// One item per line, the line's text.
    Lines,
}

impl ItemFormat {
    pub const ALL: [ItemFormat; 4] = [
        ItemFormat::Jsonl,
        ItemFormat::Json,
        ItemFormat::Csv,
        ItemFormat::Lines,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ItemFormat::Jsonl => "jsonl",
            ItemFormat::Json => "json",
            ItemFormat::Csv => "csv",
            ItemFormat::Lines => "lines",
        }
    }

    /// The format a file's extension names: `.jsonl` or `.ndjson`, `.json`,
    /// `.csv`, `.txt`; `None` for any other.
    pub fn for_path(path: &str) -> Option<ItemFormat> {
        let lower = path.to_ascii_lowercase();
        let extension = lower.rsplit_once('.').map(|(_, e)| e)?;
        match extension {
            "jsonl" | "ndjson" => Some(ItemFormat::Jsonl),
            "json" => Some(ItemFormat::Json),
            "csv" => Some(ItemFormat::Csv),
            "txt" => Some(ItemFormat::Lines),
            _ => None,
        }
    }

    /// The format of text with no other clue: a JSON array when it starts
    /// with `[`, JSON lines when it starts with `{`, else lines. CSV is
    /// never guessed.
    pub fn detect(text: &str) -> ItemFormat {
        match text.trim_start().chars().next() {
            Some('[') => ItemFormat::Json,
            Some('{') => ItemFormat::Jsonl,
            _ => ItemFormat::Lines,
        }
    }
}

impl fmt::Display for ItemFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ItemFormat {
    type Err = Error;
    fn from_str(text: &str) -> Result<ItemFormat, Error> {
        ItemFormat::ALL
            .into_iter()
            .find(|f| f.as_str() == text)
            .ok_or_else(|| {
                Error::Unsupported(format!(
                    "{text:?} is not an item format; use one of {}",
                    ItemFormat::ALL.map(ItemFormat::as_str).join(", ")
                ))
            })
    }
}

/// One item of a map: its stable id and its value.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MapItem {
    /// The item's `id` field (a string or a whole number) when it has one,
    /// else the first 12 hexadecimal digits of the BLAKE3 hash of its
    /// value, with object keys sorted. A rerun recognizes an item by it.
    pub id: String,
    pub value: Value,
}

/// Most characters in an explicit id.
const ID_MAX: usize = 100;

/// `value`'s id; see [`MapItem::id`].
pub fn item_id(value: &Value) -> Result<String, String> {
    match value.get("id") {
        Some(Value::String(id)) if !id.trim().is_empty() && id.chars().count() <= ID_MAX => {
            Ok(id.trim().to_owned())
        }
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => Ok(n.to_string()),
        Some(other) => Err(format!(
            "its id must be a non-empty string of at most {ID_MAX} characters or a whole \
             number, not {other}"
        )),
        None => {
            let digest = blake3::hash(canonical(value).as_bytes());
            Ok(digest.to_hex()[..12].to_owned())
        }
    }
}

/// `value` as compact JSON with every object's keys sorted, whatever
/// order serde_json keeps them in.
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let fields: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical(&map[k])))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(list) => format!(
            "[{}]",
            list.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        other => other.to_string(),
    }
}

/// Parse `text` as items in `format`, each with its id. Blank lines are
/// skipped (except inside a quoted CSV field). Two items with one id are
/// refused: give each an `id`.
pub fn parse_items(text: &str, format: ItemFormat) -> Result<Vec<MapItem>, Error> {
    let bad = |what: String| Error::Unsupported(format!("the map's items: {what}"));
    let values: Vec<(String, Value)> = match format {
        ItemFormat::Jsonl => {
            let mut values = Vec::new();
            for (n, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let value: Value = serde_json::from_str(line)
                    .map_err(|e| bad(format!("line {} is not JSON: {e}", n + 1)))?;
                values.push((format!("line {}", n + 1), value));
            }
            values
        }
        ItemFormat::Json => {
            let value: Value =
                serde_json::from_str(text).map_err(|e| bad(format!("not JSON: {e}")))?;
            let Value::Array(list) = value else {
                return Err(bad("a JSON file of items must hold one array".into()));
            };
            list.into_iter()
                .enumerate()
                .map(|(n, v)| (format!("item {}", n + 1), v))
                .collect()
        }
        ItemFormat::Csv => {
            let rows = parse_csv(text).map_err(bad)?;
            let mut rows = rows.into_iter();
            let Some((_, header)) = rows.next() else {
                return Ok(Vec::new());
            };
            if header.iter().any(|h| h.trim().is_empty()) {
                return Err(bad("the CSV header has an empty column name".into()));
            }
            for (i, name) in header.iter().enumerate() {
                if header[..i].contains(name) {
                    return Err(bad(format!("the CSV header names {name:?} twice")));
                }
            }
            let mut values = Vec::new();
            for (line, row) in rows {
                if row.len() != header.len() {
                    return Err(bad(format!(
                        "CSV line {line} has {} field(s); the header has {}",
                        row.len(),
                        header.len()
                    )));
                }
                let object: Map<String, Value> = header
                    .iter()
                    .cloned()
                    .zip(row.into_iter().map(Value::String))
                    .collect();
                values.push((format!("line {line}"), Value::Object(object)));
            }
            values
        }
        ItemFormat::Lines => text
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(n, l)| {
                (
                    format!("line {}", n + 1),
                    Value::String(l.trim_end().to_owned()),
                )
            })
            .collect(),
    };
    let mut items: Vec<MapItem> = Vec::with_capacity(values.len());
    let mut where_of: std::collections::BTreeMap<String, String> = Default::default();
    for (place, value) in values {
        let id = item_id(&value).map_err(|e| bad(format!("{place}: {e}")))?;
        if let Some(first) = where_of.get(&id) {
            return Err(bad(format!(
                "{place} has the id {id:?}, as {first} does; give each item a distinct \"id\""
            )));
        }
        where_of.insert(id.clone(), place);
        items.push(MapItem { id, value });
    }
    Ok(items)
}

/// Rows of CSV (RFC 4180: quoted fields may hold commas, newlines and
/// doubled quotes), each with the line it starts on. Blank lines outside
/// quotes are skipped.
pub fn parse_csv(text: &str) -> Result<Vec<(usize, Vec<String>)>, String> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut line = 1;
    let mut start = 1;
    let mut chars = text.chars().peekable();
    let end_row = |row: &mut Vec<String>, field: &mut String, rows: &mut Vec<_>, start: usize| {
        row.push(std::mem::take(field));
        let done = std::mem::take(row);
        if !(done.len() == 1 && done[0].is_empty()) {
            rows.push((start, done));
        }
    };
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                '\n' => {
                    line += 1;
                    field.push('\n');
                }
                other => field.push(other),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() && !was_quoted => {
                quoted = true;
                was_quoted = true;
            }
            '"' => return Err(format!("CSV line {line}: a quote inside an unquoted field")),
            ',' => {
                row.push(std::mem::take(&mut field));
                was_quoted = false;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' => {
                if was_quoted || !field.is_empty() || !row.is_empty() {
                    end_row(&mut row, &mut field, &mut rows, start);
                }
                was_quoted = false;
                line += 1;
                start = line;
            }
            other if was_quoted => {
                return Err(format!(
                    "CSV line {line}: {other:?} after a closing quote; expected a comma"
                ))
            }
            other => field.push(other),
        }
    }
    if quoted {
        return Err(format!("CSV line {start}: a quoted field is never closed"));
    }
    if was_quoted || !field.is_empty() || !row.is_empty() {
        end_row(&mut row, &mut field, &mut rows, start);
    }
    Ok(rows)
}

/// One CSV field, quoted when it needs to be.
pub fn csv_field(text: &str) -> String {
    match text.contains([',', '"', '\n', '\r']) {
        true => format!("\"{}\"", text.replace('"', "\"\"")),
        false => text.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// The prompt template

/// What a template is rendered with besides the item.
pub struct TemplateContext<'a> {
    pub map: &'a str,
    /// The item's position in the list, from 1.
    pub index: usize,
}

/// The placeholders in `template`, in order: (start, end, trimmed name).
fn placeholders(template: &str) -> Result<Vec<(usize, usize, String)>, String> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(open) = template[at..].find("{{") {
        let start = at + open;
        let Some(close) = template[start + 2..].find("}}") else {
            return Err(format!(
                "a placeholder opened at character {start} is never closed with }}}}"
            ));
        };
        let end = start + 2 + close + 2;
        out.push((start, end, template[start + 2..end - 2].trim().to_owned()));
        at = end;
    }
    Ok(out)
}

/// Check every placeholder of `template` is one a map has: `{{item}}`,
/// `{{item.<path>}}`, `{{id}}`, `{{index}}` or `{{map}}`.
pub fn check_template(template: &str) -> Result<(), String> {
    if template.trim().is_empty() {
        return Err("the prompt is empty".into());
    }
    for (_, _, name) in placeholders(template)? {
        let known = match name.split_once('.') {
            Some(("item", path)) => !path.is_empty() && path.split('.').all(|s| !s.is_empty()),
            None => matches!(name.as_str(), "item" | "id" | "index" | "map"),
            _ => false,
        };
        if !known {
            return Err(format!(
                "{{{{{name}}}}} is not a placeholder; use item, item.<field>[.<field>...], id, \
                 index or map"
            ));
        }
    }
    Ok(())
}

/// A value as a prompt shows it: a string as is, anything else as JSON.
fn shown(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// `template` with `item`'s values in place. A path into the item that
/// leads nowhere is an error, so a typo is found before any branch runs.
pub fn render(template: &str, item: &MapItem, context: &TemplateContext) -> Result<String, String> {
    let mut out = String::new();
    let mut at = 0;
    for (start, end, name) in placeholders(template)? {
        out.push_str(&template[at..start]);
        let value = match name.split_once('.') {
            Some(("item", path)) => {
                let mut value = &item.value;
                for step in path.split('.') {
                    let next = match value {
                        Value::Object(map) => map.get(step),
                        Value::Array(list) => step.parse::<usize>().ok().and_then(|i| list.get(i)),
                        _ => None,
                    };
                    value = next
                        .ok_or_else(|| format!("item {:?} has no {{{{item.{path}}}}}", item.id))?;
                }
                shown(value)
            }
            None if name == "item" => shown(&item.value),
            None if name == "id" => item.id.clone(),
            None if name == "index" => context.index.to_string(),
            None if name == "map" => context.map.to_owned(),
            _ => return Err(format!("{{{{{name}}}}} is not a placeholder")),
        };
        out.push_str(&value);
        at = end;
    }
    out.push_str(&template[at..]);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Answers

/// The JSON a branch answered with, from its last turn's reply: the whole
/// reply as JSON, or the reply as one fenced block, or else the last
/// fenced block in the reply (` ```json ` or plain ` ``` `). Anything else
/// is an error saying what was expected.
pub fn parse_answer(reply: &str) -> Result<Value, String> {
    let text = reply.trim();
    if text.is_empty() {
        return Err("the reply is empty; expected one JSON value".into());
    }
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        return Ok(value);
    }
    let blocks = fenced_blocks(text);
    let Some(last) = blocks.last() else {
        return Err(
            "the reply is not JSON and has no fenced ```json block; end the reply with the JSON \
             answer alone in one"
                .into(),
        );
    };
    serde_json::from_str::<Value>(last.trim())
        .map_err(|e| format!("the last fenced block of the reply is not JSON: {e}"))
}

/// The contents of every fenced block in `text`, in order.
fn fenced_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut open: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        match open.as_mut() {
            None if trimmed.starts_with("```") => open = Some(String::new()),
            None => {}
            Some(_) if trimmed == "```" => blocks.push(open.take().unwrap_or_default()),
            Some(block) => {
                block.push_str(line);
                block.push('\n');
            }
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn each_format_parses_with_ids() {
        let jsonl = parse_items(
            "{\"id\": \"a\", \"n\": 1}\n\n{\"id\": 7}\n{\"n\": 3, \"m\": [1]}\n",
            ItemFormat::Jsonl,
        )
        .unwrap();
        assert_eq!(jsonl.len(), 3);
        assert_eq!(jsonl[0].id, "a");
        assert_eq!(jsonl[1].id, "7");
        assert_eq!(jsonl[2].id.len(), 12);
        // The hash ignores key order.
        let swapped = parse_items("{\"m\": [1], \"n\": 3}", ItemFormat::Jsonl).unwrap();
        assert_eq!(swapped[0].id, jsonl[2].id);

        let json = parse_items("[\"x\", {\"id\": \"y\"}]", ItemFormat::Json).unwrap();
        assert_eq!(json[1].id, "y");
        assert_eq!(json[0].value, json!("x"));

        let csv = parse_items(
            "id,name,note\r\n1,ripgrep,\"fast, \"\"very\"\"\"\n2,fd,\"two\nlines\"\n\n",
            ItemFormat::Csv,
        )
        .unwrap();
        assert_eq!(csv.len(), 2);
        assert_eq!(csv[0].value["note"], "fast, \"very\"");
        assert_eq!(csv[1].value["note"], "two\nlines");
        assert_eq!(csv[1].id, "2");

        let lines = parse_items("alpha\n\n  beta  \n", ItemFormat::Lines).unwrap();
        assert_eq!(lines[1].value, json!("  beta"));
    }

    #[test]
    fn bad_items_are_refused_with_where() {
        let cases = [
            (
                "{\"id\": \"a\"}\n{\"id\": \"a\"}",
                ItemFormat::Jsonl,
                "line 2 has the id \"a\", as line 1 does",
            ),
            (
                "{\"a\": 1}\nnot json",
                ItemFormat::Jsonl,
                "line 2 is not JSON",
            ),
            ("{\"a\": 1}", ItemFormat::Json, "must hold one array"),
            (
                "a,b\n1\n",
                ItemFormat::Csv,
                "CSV line 2 has 1 field(s); the header has 2",
            ),
            ("a,a\n1,2\n", ItemFormat::Csv, "names \"a\" twice"),
            ("a\n\"open\n", ItemFormat::Csv, "never closed"),
            (
                "[{\"id\": true}]",
                ItemFormat::Json,
                "item 1: its id must be",
            ),
            (
                "same\nsame\n",
                ItemFormat::Lines,
                "give each item a distinct",
            ),
        ];
        for (text, format, expected) in cases {
            let error = parse_items(text, format).unwrap_err().to_string();
            assert!(error.contains(expected), "{expected}: {error}");
        }
    }

    #[test]
    fn formats_come_from_extensions_or_the_text() {
        assert_eq!(ItemFormat::for_path("a/b.NDJSON"), Some(ItemFormat::Jsonl));
        assert_eq!(ItemFormat::for_path("x.csv"), Some(ItemFormat::Csv));
        assert_eq!(ItemFormat::for_path("x"), None);
        assert_eq!(ItemFormat::detect("  [1]"), ItemFormat::Json);
        assert_eq!(ItemFormat::detect("{}\n{}"), ItemFormat::Jsonl);
        assert_eq!(ItemFormat::detect("a,b"), ItemFormat::Lines);
        assert!("xml".parse::<ItemFormat>().is_err());
    }

    #[test]
    fn templates_substitute_fields_and_refuse_unknowns() {
        let item = MapItem {
            id: "rg".into(),
            value: json!({"name": "ripgrep", "meta": {"tags": ["cli", "search"]}, "stars": 5}),
        };
        let context = TemplateContext { map: "m", index: 2 };
        let text = render(
            "{{ item.name }} ({{id}}, #{{index}} of {{map}}): {{item.meta.tags.1}} {{item.stars}} {{item.meta}}",
            &item,
            &context,
        )
        .unwrap();
        assert_eq!(
            text,
            "ripgrep (rg, #2 of m): search 5 {\"tags\":[\"cli\",\"search\"]}"
        );
        let error = render("{{item.nope}}", &item, &context).unwrap_err();
        assert!(
            error.contains("item \"rg\" has no {{item.nope}}"),
            "{error}"
        );
        assert!(check_template("{{item.a.b}} {{id}}").is_ok());
        assert!(check_template("{{event.x}}")
            .unwrap_err()
            .contains("not a placeholder"));
        assert!(check_template("{{item.}}").is_err());
        assert!(check_template("{{item")
            .unwrap_err()
            .contains("never closed"));
        assert!(check_template("  ").is_err());
        let line = MapItem {
            id: "x".into(),
            value: json!("just text"),
        };
        assert_eq!(
            render("<{{item}}>", &line, &context).unwrap(),
            "<just text>"
        );
    }

    #[test]
    fn answers_are_whole_json_or_the_last_fenced_block() {
        assert_eq!(parse_answer(" {\"a\": 1} ").unwrap(), json!({"a": 1}));
        assert_eq!(
            parse_answer("```json\n{\"a\": 2}\n```").unwrap(),
            json!({"a": 2})
        );
        assert_eq!(
            parse_answer("I looked.\n```json\n{\"a\": 1}\n```\nThen:\n```\n{\"a\": 3}\n```\n")
                .unwrap(),
            json!({"a": 3})
        );
        assert!(parse_answer("echo: hi").unwrap_err().contains("no fenced"));
        assert!(parse_answer("```json\n{oops\n```")
            .unwrap_err()
            .contains("not JSON"));
        assert!(parse_answer("").unwrap_err().contains("empty"));
    }

    #[test]
    fn csv_fields_are_quoted_when_needed() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        let rows = parse_csv(&format!("{},{}\n", csv_field("x,\"y\""), csv_field("z"))).unwrap();
        assert_eq!(rows[0].1, ["x,\"y\"", "z"]);
    }
}
