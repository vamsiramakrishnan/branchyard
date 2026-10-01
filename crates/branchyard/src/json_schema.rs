//! A subset of JSON Schema, checked offline with no dependency, for the
//! answers of a map's branches (`docs/map.md#schemas`).
//!
//! Supported keywords: `type` (one name or a list), `properties`,
//! `required`, `additionalProperties` (a boolean or a schema), `items` (a
//! schema), `enum`, `const`, `minimum`, `maximum`, `exclusiveMinimum`,
//! `exclusiveMaximum` (numbers), `minLength`, `maxLength`, `minItems`,
//! `maxItems`, `uniqueItems` and `anyOf`. The annotations `$schema`, `$id`,
//! `$comment`, `title`, `description`, `default`, `examples` and `format`
//! are allowed and not checked (`format` is an annotation in JSON Schema
//! 2020-12 too). Any other keyword, such as `pattern`, `$ref` or `oneOf`,
//! is refused when the schema is loaded, so nothing a schema says is
//! silently left unchecked. A schema may also be `true` or `false`.

use serde_json::{Map, Value};

/// Keywords checked.
const CHECKED: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "uniqueItems",
    "anyOf",
];
/// Keywords allowed and ignored.
const ANNOTATIONS: &[&str] = &[
    "$schema",
    "$id",
    "$comment",
    "title",
    "description",
    "default",
    "examples",
    "format",
];
const TYPES: &[&str] = &[
    "null", "boolean", "object", "array", "number", "integer", "string",
];
/// Errors reported at most for one value.
const ERRORS_MAX: usize = 20;

/// A schema in the supported subset, checked when built.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonSchema {
    root: Value,
}

impl JsonSchema {
    /// Check that `schema` uses only the supported keywords, each well
    /// formed.
    pub fn new(schema: Value) -> Result<JsonSchema, String> {
        check_schema(&schema, "$")?;
        Ok(JsonSchema { root: schema })
    }

    /// The schema as given.
    pub fn value(&self) -> &Value {
        &self.root
    }

    /// The top-level object's property names, in the schema's order.
    pub fn properties(&self) -> Vec<String> {
        self.root
            .get("properties")
            .and_then(Value::as_object)
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Every way `value` fails the schema (at most 20), each naming where:
    /// `$` is the value, `$.a[0]` a field's first item. Empty when it
    /// matches.
    pub fn validate(&self, value: &Value) -> Vec<String> {
        let mut errors = Vec::new();
        validate(&self.root, value, "$", &mut errors);
        errors.truncate(ERRORS_MAX);
        errors
    }
}

fn number(schema: &Map<String, Value>, key: &str, at: &str) -> Result<Option<f64>, String> {
    match schema.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| format!("{at}: {key} must be a number")),
    }
}

fn count(schema: &Map<String, Value>, key: &str, at: &str) -> Result<Option<u64>, String> {
    match schema.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{at}: {key} must be a whole number of at least 0")),
    }
}

fn check_schema(schema: &Value, at: &str) -> Result<(), String> {
    let object = match schema {
        Value::Bool(_) => return Ok(()),
        Value::Object(object) => object,
        _ => return Err(format!("{at}: a schema must be an object or a boolean")),
    };
    for key in object.keys() {
        if !CHECKED.contains(&key.as_str()) && !ANNOTATIONS.contains(&key.as_str()) {
            return Err(format!(
                "{at}: {key:?} is not supported; a map's schema may use {} (see docs/map.md)",
                CHECKED.join(", ")
            ));
        }
    }
    if let Some(types) = object.get("type") {
        let names: Vec<&Value> = match types {
            Value::Array(list) if !list.is_empty() => list.iter().collect(),
            Value::String(_) => vec![types],
            _ => return Err(format!("{at}: type must be a name or a list of names")),
        };
        for name in names {
            if !name.as_str().is_some_and(|n| TYPES.contains(&n)) {
                return Err(format!(
                    "{at}: type {name} is not one of {}",
                    TYPES.join(", ")
                ));
            }
        }
    }
    if let Some(properties) = object.get("properties") {
        let properties = properties
            .as_object()
            .ok_or_else(|| format!("{at}: properties must be an object"))?;
        for (name, sub) in properties {
            check_schema(sub, &format!("{at}.properties.{name}"))?;
        }
    }
    if let Some(required) = object.get("required") {
        let ok = required
            .as_array()
            .is_some_and(|list| list.iter().all(Value::is_string));
        if !ok {
            return Err(format!("{at}: required must be a list of names"));
        }
    }
    if let Some(additional) = object.get("additionalProperties") {
        check_schema(additional, &format!("{at}.additionalProperties"))?;
    }
    if let Some(items) = object.get("items") {
        check_schema(items, &format!("{at}.items"))?;
    }
    if let Some(any) = object.get("anyOf") {
        let list = any
            .as_array()
            .filter(|l| !l.is_empty())
            .ok_or_else(|| format!("{at}: anyOf must be a list of schemas"))?;
        for (n, sub) in list.iter().enumerate() {
            check_schema(sub, &format!("{at}.anyOf[{n}]"))?;
        }
    }
    if object.get("enum").is_some_and(|e| !e.is_array()) {
        return Err(format!("{at}: enum must be a list"));
    }
    if object.get("uniqueItems").is_some_and(|u| !u.is_boolean()) {
        return Err(format!("{at}: uniqueItems must be true or false"));
    }
    for key in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
        number(object, key, at)?;
    }
    for key in ["minLength", "maxLength", "minItems", "maxItems"] {
        count(object, key, at)?;
    }
    Ok(())
}

fn type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Object(_) => "object",
        Value::Array(_) => "array",
        Value::Number(_) => "number",
        Value::String(_) => "string",
    }
}

fn is_type(value: &Value, name: &str) -> bool {
    match name {
        "integer" => match value {
            Value::Number(n) => {
                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            _ => false,
        },
        "number" => value.is_number(),
        other => type_of(value) == other,
    }
}

/// Numbers compare by value (`1` equals `1.0`), everything else exactly.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

fn short(value: &Value) -> String {
    let text = value.to_string();
    match text.chars().count() > 60 {
        true => format!("{}...", text.chars().take(60).collect::<String>()),
        false => text,
    }
}

fn validate(schema: &Value, value: &Value, at: &str, errors: &mut Vec<String>) {
    if errors.len() >= ERRORS_MAX {
        return;
    }
    let object = match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            errors.push(format!("{at}: no value is allowed here"));
            return;
        }
        Value::Object(object) => object,
        _ => return,
    };
    if let Some(types) = object.get("type") {
        let names: Vec<&str> = match types {
            Value::String(name) => vec![name.as_str()],
            Value::Array(list) => list.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !names.iter().any(|n| is_type(value, n)) {
            errors.push(format!(
                "{at}: expected {}, got {}",
                names.join(" or "),
                type_of(value)
            ));
            return;
        }
    }
    if let Some(allowed) = object.get("enum").and_then(Value::as_array) {
        if !allowed.iter().any(|a| same(a, value)) {
            let list: Vec<String> = allowed.iter().map(short).collect();
            errors.push(format!(
                "{at}: {} is not one of {}",
                short(value),
                list.join(", ")
            ));
        }
    }
    if let Some(expected) = object.get("const") {
        if !same(expected, value) {
            errors.push(format!("{at}: must be {}", short(expected)));
        }
    }
    if let Some(list) = object.get("anyOf").and_then(Value::as_array) {
        let matched = list.iter().any(|sub| {
            let mut sub_errors = Vec::new();
            validate(sub, value, at, &mut sub_errors);
            sub_errors.is_empty()
        });
        if !matched {
            errors.push(format!("{at}: matches none of anyOf's schemas"));
        }
    }
    match value {
        Value::Number(n) => {
            let n = n.as_f64().unwrap_or(f64::NAN);
            let bound = |key: &str| object.get(key).and_then(Value::as_f64);
            if let Some(min) = bound("minimum").filter(|m| n < *m) {
                errors.push(format!("{at}: {n} is less than the minimum {min}"));
            }
            if let Some(max) = bound("maximum").filter(|m| n > *m) {
                errors.push(format!("{at}: {n} is more than the maximum {max}"));
            }
            if let Some(min) = bound("exclusiveMinimum").filter(|m| n <= *m) {
                errors.push(format!("{at}: {n} must be more than {min}"));
            }
            if let Some(max) = bound("exclusiveMaximum").filter(|m| n >= *m) {
                errors.push(format!("{at}: {n} must be less than {max}"));
            }
        }
        Value::String(text) => {
            let length = text.chars().count() as u64;
            let bound = |key: &str| object.get(key).and_then(Value::as_u64);
            if let Some(min) = bound("minLength").filter(|m| length < *m) {
                errors.push(format!(
                    "{at}: {length} character(s), fewer than the minimum {min}"
                ));
            }
            if let Some(max) = bound("maxLength").filter(|m| length > *m) {
                errors.push(format!(
                    "{at}: {length} character(s), more than the maximum {max}"
                ));
            }
        }
        Value::Array(list) => {
            let length = list.len() as u64;
            let bound = |key: &str| object.get(key).and_then(Value::as_u64);
            if let Some(min) = bound("minItems").filter(|m| length < *m) {
                errors.push(format!(
                    "{at}: {length} item(s), fewer than the minimum {min}"
                ));
            }
            if let Some(max) = bound("maxItems").filter(|m| length > *m) {
                errors.push(format!(
                    "{at}: {length} item(s), more than the maximum {max}"
                ));
            }
            if object.get("uniqueItems") == Some(&Value::Bool(true)) {
                for (i, a) in list.iter().enumerate() {
                    if list[..i].iter().any(|b| same(a, b)) {
                        errors.push(format!("{at}[{i}]: repeats an earlier item"));
                        break;
                    }
                }
            }
            if let Some(items) = object.get("items") {
                for (i, item) in list.iter().enumerate() {
                    validate(items, item, &format!("{at}[{i}]"), errors);
                }
            }
        }
        Value::Object(fields) => {
            if let Some(required) = object.get("required").and_then(Value::as_array) {
                for name in required.iter().filter_map(Value::as_str) {
                    if !fields.contains_key(name) {
                        errors.push(format!("{at}: missing the required field {name:?}"));
                    }
                }
            }
            let properties = object.get("properties").and_then(Value::as_object);
            for (name, field) in fields {
                let path = format!("{at}.{name}");
                match properties.and_then(|p| p.get(name)) {
                    Some(sub) => validate(sub, field, &path, errors),
                    None => match object.get("additionalProperties") {
                        Some(Value::Bool(false)) => {
                            errors.push(format!("{at}: the field {name:?} is not allowed"))
                        }
                        Some(sub) => validate(sub, field, &path, errors),
                        None => {}
                    },
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> JsonSchema {
        JsonSchema::new(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "required": ["name", "stars"],
            "additionalProperties": false,
            "properties": {
                "name": {"type": "string", "minLength": 1, "description": "the project"},
                "stars": {"type": "integer", "minimum": 0},
                "license": {"enum": ["MIT", "Apache-2.0", null]},
                "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 3, "uniqueItems": true},
                "score": {"anyOf": [{"type": "number", "maximum": 1}, {"const": "n/a"}]}
            }
        }))
        .unwrap()
    }

    #[test]
    fn a_matching_value_has_no_errors() {
        let s = schema();
        assert!(s.validate(&json!({"name": "x", "stars": 3})).is_empty());
        assert!(s
            .validate(
                &json!({"name": "x", "stars": 3.0, "license": null, "tags": ["a"], "score": "n/a"})
            )
            .is_empty());
        assert!(s
            .validate(&json!({"name": "x", "stars": 0, "score": 0.5}))
            .is_empty());
    }

    #[test]
    fn each_failure_names_its_place() {
        let s = schema();
        let errors = s.validate(&json!({
            "name": "", "stars": -1.5, "license": "GPL", "tags": ["a", "a", 3, "d"],
            "score": 2, "extra": true
        }));
        let text = errors.join("\n");
        for expected in [
            "$.name: 0 character(s), fewer than the minimum 1",
            "$.stars: expected integer, got number",
            "$.license: \"GPL\" is not one of \"MIT\", \"Apache-2.0\", null",
            "$.tags: 4 item(s), more than the maximum 3",
            "$.tags[1]: repeats an earlier item",
            "$.tags[2]: expected string, got number",
            "$.score: matches none of anyOf's schemas",
            "$: the field \"extra\" is not allowed",
        ] {
            assert!(text.contains(expected), "{expected}\n{text}");
        }
        let missing = s.validate(&json!({"stars": 1}));
        assert_eq!(missing, ["$: missing the required field \"name\""]);
        assert_eq!(s.validate(&json!([1])), ["$: expected object, got array"]);
        // In the schema's order where serde_json keeps it, so compare as sets.
        let mut names = s.properties();
        names.sort();
        assert_eq!(names, ["license", "name", "score", "stars", "tags"]);
    }

    #[test]
    fn keywords_outside_the_subset_are_refused_when_loaded() {
        for (bad, expected) in [
            (
                json!({"type": "string", "pattern": "^a"}),
                "\"pattern\" is not supported",
            ),
            (
                json!({"properties": {"a": {"$ref": "#/x"}}}),
                "$.properties.a: \"$ref\"",
            ),
            (json!({"type": "text"}), "type \"text\" is not one of"),
            (json!({"minLength": -1}), "minLength must be a whole number"),
            (json!({"required": "a"}), "required must be a list of names"),
            (json!(3), "must be an object or a boolean"),
        ] {
            let error = JsonSchema::new(bad).unwrap_err();
            assert!(error.contains(expected), "{expected}: {error}");
        }
        assert!(JsonSchema::new(json!(true))
            .unwrap()
            .validate(&json!(1))
            .is_empty());
        assert_eq!(
            JsonSchema::new(json!(false)).unwrap().validate(&json!(1)),
            ["$: no value is allowed here"]
        );
    }
}
