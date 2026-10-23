//! A subset of JSON Schema, checked offline by Branchyard's own code: the
//! one checker behind a map's answers (`docs/map.md#schemas`) and a
//! result contract (`docs/delegation.md#result-contract`).
//!
//! Checked keywords ([`KEYWORDS`]): `type` (one name or a list),
//! `properties`, `required`, `additionalProperties` (a boolean or a
//! schema), `items` (one schema), `enum`, `const`, `minimum`, `maximum`,
//! `exclusiveMinimum`, `exclusiveMaximum` (numbers, the 2020-12 form),
//! `minLength`, `maxLength`, `pattern` (a regular expression searched
//! anywhere in the string, as in JSON Schema), `minItems`, `maxItems`,
//! `uniqueItems`, `anyOf` and `oneOf`. The annotations ([`ANNOTATIONS`])
//! `$schema`, `$id`, `$comment`, `title`, `description`, `default`,
//! `examples`, `deprecated`, `readOnly` and `writeOnly` are allowed and not
//! checked; a map's schema also takes `format` as one (it is an annotation
//! in JSON Schema 2020-12 too), a contract's refuses it. Any other keyword,
//! such as `$ref`, `allOf`, `not`, `if` or `patternProperties`, is refused
//! when the schema is declared, by name, so nothing a schema says is
//! silently left unchecked. So is a keyword of the right name in the wrong
//! form, and bounds no value could meet (`minimum` above `maximum`). A
//! schema may also be `true` or `false`.

use std::collections::HashMap;

use regex::Regex;
use serde_json::{Map, Value};

/// Keywords checked.
pub(crate) const KEYWORDS: &[&str] = &[
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
    "pattern",
    "minItems",
    "maxItems",
    "uniqueItems",
    "anyOf",
    "oneOf",
];
/// Keywords allowed and ignored, wherever a schema is declared.
pub(crate) const ANNOTATIONS: &[&str] = &[
    "$schema",
    "$id",
    "$comment",
    "title",
    "description",
    "default",
    "examples",
    "deprecated",
    "readOnly",
    "writeOnly",
];
/// The names `type` may take.
pub(crate) const TYPES: &[&str] = &[
    "object", "array", "string", "number", "integer", "boolean", "null",
];
/// Errors reported at most for one value.
const ERRORS_MAX: usize = 20;
/// Errors of the closest alternative shown when none of `anyOf` or `oneOf`
/// matches.
const CLOSEST_SHOWN: usize = 2;

/// Where a schema is declared: it names the root in a refusal, says who
/// refuses, and decides whether `format` is an annotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dialect {
    /// A map's `--schema`: the root is `$`, and `format` is an annotation.
    Map,
    /// A result contract's schema: the root is `result.schema`, and
    /// `format` is refused, since a contract checks everything it says.
    Contract,
}

impl Dialect {
    fn root(self) -> &'static str {
        match self {
            Dialect::Map => "$",
            Dialect::Contract => "result.schema",
        }
    }

    fn who(self) -> &'static str {
        match self {
            Dialect::Map => "a map's schema",
            Dialect::Contract => "the contract",
        }
    }

    fn allows(self, key: &str) -> bool {
        KEYWORDS.contains(&key)
            || ANNOTATIONS.contains(&key)
            || (key == "format" && self == Dialect::Map)
    }
}

/// A schema in the supported subset, checked when built.
#[derive(Clone, Debug)]
pub struct JsonSchema {
    root: Value,
    /// Every `pattern` in the schema, compiled once.
    patterns: HashMap<String, Regex>,
}

impl PartialEq for JsonSchema {
    fn eq(&self, other: &JsonSchema) -> bool {
        self.root == other.root
    }
}

impl JsonSchema {
    /// Check that `schema` uses only the supported keywords, each well
    /// formed, as a map's schema: a refusal names the place from `$`
    /// (`$.properties.id.pattern is not a valid regular expression: ...`).
    pub fn new(schema: Value) -> Result<JsonSchema, String> {
        JsonSchema::declared(schema, Dialect::Map)
    }

    /// [`JsonSchema::new`] as `dialect` declares it.
    pub(crate) fn declared(schema: Value, dialect: Dialect) -> Result<JsonSchema, String> {
        let mut patterns = HashMap::new();
        check_schema(&schema, dialect.root(), dialect, &mut patterns)?;
        Ok(JsonSchema {
            root: schema,
            patterns,
        })
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
        self.validate_at(value, "$")
    }

    /// [`JsonSchema::validate`], naming the value `root` instead of `$`:
    /// with `result`, `result.a[0]` is a field's first item.
    pub fn validate_at(&self, value: &Value, root: &str) -> Vec<String> {
        let mut errors = Vec::new();
        self.check(&self.root, value, root, &mut errors);
        errors.truncate(ERRORS_MAX);
        errors
    }

    /// Whether `value` matches the schema.
    pub fn accepts(&self, value: &Value) -> bool {
        self.validate(value).is_empty()
    }
}

/// `value` cut to 60 characters, for a message.
fn short(value: &Value) -> String {
    let text = value.to_string();
    match text.chars().count() > 60 {
        true => format!("{}...", text.chars().take(60).collect::<String>()),
        false => text,
    }
}

fn number(schema: &Map<String, Value>, key: &str, at: &str) -> Result<Option<f64>, String> {
    match schema.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .map(Some)
            .ok_or_else(|| format!("{at}.{key} must be a number")),
    }
}

fn count(schema: &Map<String, Value>, key: &str, at: &str) -> Result<Option<u64>, String> {
    match schema.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{at}.{key} must be a whole number of at least 0")),
    }
}

/// `low` above `high` (or at it, when `strict`), refused naming both.
fn ordered<T: PartialOrd + std::fmt::Display>(
    (low_key, low): (&str, Option<T>),
    (high_key, high): (&str, Option<T>),
    strict: bool,
    at: &str,
) -> Result<(), String> {
    let (Some(low), Some(high)) = (low, high) else {
        return Ok(());
    };
    let empty = match strict {
        true => low >= high,
        false => low > high,
    };
    match empty {
        true => Err(format!(
            "{at}.{low_key} ({low}) is not below {at}.{high_key} ({high}), so no value could \
             match"
        )),
        false => Ok(()),
    }
}

fn check_schema(
    schema: &Value,
    at: &str,
    dialect: Dialect,
    patterns: &mut HashMap<String, Regex>,
) -> Result<(), String> {
    let object = match schema {
        Value::Bool(_) => return Ok(()),
        Value::Object(object) => object,
        _ => {
            return Err(format!(
                "{at} must be a schema (an object, or true or false), not {}",
                short(schema)
            ))
        }
    };
    if let Some(key) = object.keys().find(|k| !dialect.allows(k)) {
        return Err(format!(
            "{at} uses `{key}`, which {} does not support",
            dialect.who()
        ));
    }
    check_type(object, at)?;
    if let Some(properties) = object.get("properties") {
        let properties = properties
            .as_object()
            .ok_or_else(|| format!("{at}.properties must be an object of schemas"))?;
        for (name, sub) in properties {
            check_schema(sub, &format!("{at}.properties.{name}"), dialect, patterns)?;
        }
    }
    check_required(object, at)?;
    if let Some(additional) = object.get("additionalProperties") {
        check_schema(
            additional,
            &format!("{at}.additionalProperties"),
            dialect,
            patterns,
        )?;
    }
    if let Some(items) = object.get("items") {
        if items.is_array() {
            return Err(format!(
                "{at}.items must be one schema; a list of schemas (prefixItems) is not supported"
            ));
        }
        check_schema(items, &format!("{at}.items"), dialect, patterns)?;
    }
    for key in ["anyOf", "oneOf"] {
        let Some(list) = object.get(key) else {
            continue;
        };
        let list = list
            .as_array()
            .filter(|l| !l.is_empty())
            .ok_or_else(|| format!("{at}.{key} must be a non-empty array of schemas"))?;
        for (n, sub) in list.iter().enumerate() {
            check_schema(sub, &format!("{at}.{key}[{n}]"), dialect, patterns)?;
        }
    }
    if object
        .get("enum")
        .is_some_and(|e| e.as_array().is_none_or(|list| list.is_empty()))
    {
        return Err(format!("{at}.enum must be a non-empty array of values"));
    }
    check_forms(object, at)?;
    check_bounds(object, at)?;
    if let Some(pattern) = object.get("pattern") {
        let text = pattern
            .as_str()
            .ok_or_else(|| format!("{at}.pattern must be a string"))?;
        let regex = Regex::new(text).map_err(|e| {
            format!(
                "{at}.pattern is not a valid regular expression: {}",
                regex_error(&e)
            )
        })?;
        patterns.insert(text.to_owned(), regex);
    }
    Ok(())
}

/// A regex error on one line: its `error:` line when it has one.
fn regex_error(error: &regex::Error) -> String {
    let text = error.to_string();
    text.lines()
        .find_map(|line| line.trim().strip_prefix("error: "))
        .map_or_else(
            || text.split_whitespace().collect::<Vec<_>>().join(" "),
            str::to_owned,
        )
}

fn check_type(object: &Map<String, Value>, at: &str) -> Result<(), String> {
    let Some(types) = object.get("type") else {
        return Ok(());
    };
    let bad = || {
        format!(
            "{at}.type must be one of {}, or a non-empty list of them, not {}",
            TYPES.join(", "),
            short(types)
        )
    };
    let names: Vec<&str> = match types {
        Value::String(name) => vec![name.as_str()],
        Value::Array(list) if !list.is_empty() => list
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .ok_or_else(bad)?,
        _ => return Err(bad()),
    };
    if !names.iter().all(|n| TYPES.contains(n)) {
        return Err(bad());
    }
    if let Some((i, name)) = names
        .iter()
        .enumerate()
        .find(|(i, n)| names[..*i].contains(n))
    {
        return Err(format!("{at}.type names {name:?} twice (at {i})"));
    }
    Ok(())
}

fn check_required(object: &Map<String, Value>, at: &str) -> Result<(), String> {
    let Some(required) = object.get("required") else {
        return Ok(());
    };
    let names: Option<Vec<&str>> = required
        .as_array()
        .and_then(|list| list.iter().map(Value::as_str).collect());
    let Some(names) = names else {
        return Err(format!("{at}.required must be an array of property names"));
    };
    if object.get("additionalProperties") == Some(&Value::Bool(false)) {
        let declared = object.get("properties").and_then(Value::as_object);
        if let Some(name) = names
            .iter()
            .find(|n| !declared.is_some_and(|p| p.contains_key(**n)))
        {
            return Err(format!(
                "{at}.required names `{name}`, which is not in properties while \
                 additionalProperties is false, so no value could match"
            ));
        }
    }
    Ok(())
}

/// The keywords whose value is a plain string, boolean or list.
fn check_forms(object: &Map<String, Value>, at: &str) -> Result<(), String> {
    for key in [
        "title",
        "description",
        "$comment",
        "$schema",
        "$id",
        "format",
    ] {
        if object.get(key).is_some_and(|v| !v.is_string()) {
            return Err(format!("{at}.{key} must be a string"));
        }
    }
    for key in ["uniqueItems", "deprecated", "readOnly", "writeOnly"] {
        if object.get(key).is_some_and(|v| !v.is_boolean()) {
            return Err(format!("{at}.{key} must be true or false"));
        }
    }
    if object.get("examples").is_some_and(|v| !v.is_array()) {
        return Err(format!("{at}.examples must be an array of values"));
    }
    Ok(())
}

/// Numbers and counts well formed, and each pair of bounds leaving room
/// for a value.
fn check_bounds(object: &Map<String, Value>, at: &str) -> Result<(), String> {
    let minimum = number(object, "minimum", at)?;
    let maximum = number(object, "maximum", at)?;
    let exclusive_minimum = number(object, "exclusiveMinimum", at)?;
    let exclusive_maximum = number(object, "exclusiveMaximum", at)?;
    ordered(("minimum", minimum), ("maximum", maximum), false, at)?;
    ordered(
        ("exclusiveMinimum", exclusive_minimum),
        ("exclusiveMaximum", exclusive_maximum),
        true,
        at,
    )?;
    ordered(
        ("minimum", minimum),
        ("exclusiveMaximum", exclusive_maximum),
        true,
        at,
    )?;
    ordered(
        ("exclusiveMinimum", exclusive_minimum),
        ("maximum", maximum),
        true,
        at,
    )?;
    for (low, high) in [("minLength", "maxLength"), ("minItems", "maxItems")] {
        ordered(
            (low, count(object, low, at)?),
            (high, count(object, high, at)?),
            false,
            at,
        )?;
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

/// JSON equality with numbers compared by value (`1` equals `1.0`), at
/// any depth.
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

fn alternatives(n: usize) -> String {
    match n {
        1 => "the 1 alternative".to_owned(),
        n => format!("the {n} alternatives"),
    }
}

impl JsonSchema {
    fn check(&self, schema: &Value, value: &Value, at: &str, errors: &mut Vec<String>) {
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
        for key in ["anyOf", "oneOf"] {
            if let Some(list) = object.get(key).and_then(Value::as_array) {
                self.alternatives(key, list, value, at, errors);
            }
        }
        match value {
            Value::Number(n) => number_errors(object, n.as_f64().unwrap_or(f64::NAN), at, errors),
            Value::String(text) => self.string_errors(object, text, at, errors),
            Value::Array(list) => self.array_errors(object, list, at, errors),
            Value::Object(fields) => self.object_errors(object, fields, at, errors),
            _ => {}
        }
    }

    /// `anyOf` (at least one of `list` matches) or `oneOf` (exactly one).
    /// When none matches, the error shows the closest alternative, the one
    /// with the fewest errors, and its first two.
    fn alternatives(
        &self,
        key: &str,
        list: &[Value],
        value: &Value,
        at: &str,
        errors: &mut Vec<String>,
    ) {
        let outcomes: Vec<Vec<String>> = list
            .iter()
            .map(|sub| {
                let mut found = Vec::new();
                self.check(sub, value, at, &mut found);
                found
            })
            .collect();
        let matched: Vec<usize> = (0..outcomes.len())
            .filter(|n| outcomes[*n].is_empty())
            .collect();
        if matched.is_empty() {
            // An alternative of another type is further than one of the
            // right type with any number of errors.
            let wrong_type = format!("{at}: expected ");
            let Some((n, closest)) = outcomes
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| (e.len() == 1 && e[0].starts_with(&wrong_type), e.len()))
            else {
                return;
            };
            let mut shown = closest
                .iter()
                .take(CLOSEST_SHOWN)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ");
            if closest.len() > CLOSEST_SHOWN {
                shown.push_str("; and more");
            }
            errors.push(format!(
                "{at}: matches none of {} in {key} (closest, {key}[{n}]: {shown})",
                alternatives(list.len())
            ));
        } else if key == "oneOf" && matched.len() > 1 {
            let which: Vec<String> = matched.iter().map(|n| format!("oneOf[{n}]")).collect();
            errors.push(format!(
                "{at}: matches {} of {} in oneOf ({}); exactly one must match",
                matched.len(),
                alternatives(list.len()),
                which.join(", ")
            ));
        }
    }

    fn string_errors(
        &self,
        object: &Map<String, Value>,
        text: &str,
        at: &str,
        errors: &mut Vec<String>,
    ) {
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
        if let Some(pattern) = object.get("pattern").and_then(Value::as_str) {
            if self
                .patterns
                .get(pattern)
                .is_some_and(|regex| !regex.is_match(text))
            {
                errors.push(format!(
                    "{at}: {} does not match the pattern {}",
                    short(&Value::String(text.to_owned())),
                    short(&Value::String(pattern.to_owned()))
                ));
            }
        }
    }

    fn array_errors(
        &self,
        object: &Map<String, Value>,
        list: &[Value],
        at: &str,
        errors: &mut Vec<String>,
    ) {
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
            let repeat = (0..list.len()).find_map(|i| {
                list[..i]
                    .iter()
                    .position(|b| same(&list[i], b))
                    .map(|j| (i, j))
            });
            if let Some((i, j)) = repeat {
                errors.push(format!("{at}[{i}]: repeats the item at [{j}]"));
            }
        }
        if let Some(items) = object.get("items") {
            for (i, item) in list.iter().enumerate() {
                self.check(items, item, &format!("{at}[{i}]"), errors);
            }
        }
    }

    fn object_errors(
        &self,
        object: &Map<String, Value>,
        fields: &Map<String, Value>,
        at: &str,
        errors: &mut Vec<String>,
    ) {
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
                Some(sub) => self.check(sub, field, &path, errors),
                None => match object.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        errors.push(format!("{at}: the field {name:?} is not allowed"))
                    }
                    Some(sub) => self.check(sub, field, &path, errors),
                    None => {}
                },
            }
        }
    }
}

fn number_errors(object: &Map<String, Value>, n: f64, at: &str, errors: &mut Vec<String>) {
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

    fn refused(schema: Value) -> String {
        JsonSchema::new(schema).unwrap_err()
    }

    fn contract_refused(schema: Value) -> String {
        JsonSchema::declared(schema, Dialect::Contract).unwrap_err()
    }

    fn errors(schema: Value, value: Value) -> Vec<String> {
        JsonSchema::new(schema).unwrap().validate(&value)
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
        assert!(s.accepts(&json!({"name": "x", "stars": 0})));
        assert!(!s.accepts(&json!({"name": "x"})));
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
            "$.tags[1]: repeats the item at [0]",
            "$.tags[2]: expected string, got number",
            "$.score: matches none of the 2 alternatives in anyOf (closest, anyOf[0]: \
             $.score: 2 is more than the maximum 1)",
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
    fn const_matches_one_value_with_numbers_by_value() {
        let schema = json!({"const": [{"n": 1, "m": [2]}, 1, "a"]});
        assert!(errors(schema.clone(), json!([{"m": [2.0], "n": 1.0}, 1.0, "a"])).is_empty());
        assert_eq!(
            errors(schema.clone(), json!([{"n": 1, "m": [2]}, 2, "a"])),
            ["$: must be [{\"n\":1,\"m\":[2]},1,\"a\"]"]
        );
        assert_eq!(
            errors(json!({"const": null}), json!(0)),
            ["$: must be null"]
        );
        assert!(errors(json!({"const": null}), json!(null)).is_empty());
    }

    #[test]
    fn item_counts_and_unique_items_are_checked() {
        let schema = json!({"type": "array", "minItems": 2, "maxItems": 3, "uniqueItems": true});
        assert!(errors(schema.clone(), json!([1, 2])).is_empty());
        assert!(errors(schema.clone(), json!([{"a": 1}, {"a": 2}, [1]])).is_empty());
        assert_eq!(
            errors(schema.clone(), json!([1])),
            ["$: 1 item(s), fewer than the minimum 2"]
        );
        assert_eq!(
            errors(schema.clone(), json!([1, 2, 3, 4])),
            ["$: 4 item(s), more than the maximum 3"]
        );
        // 1 and 1.0 are the same item, and so are objects equal by value.
        assert_eq!(
            errors(schema.clone(), json!([1, 1.0])),
            ["$[1]: repeats the item at [0]"]
        );
        assert_eq!(
            errors(schema.clone(), json!([{"a": [1]}, 2, {"a": [1.0]}])),
            ["$[2]: repeats the item at [0]"]
        );
        // uniqueItems false allows repeats.
        assert!(errors(json!({"uniqueItems": false}), json!([1, 1])).is_empty());
        for (bad, expected) in [
            (
                json!({"minItems": -1}),
                "$.minItems must be a whole number of at least 0",
            ),
            (
                json!({"maxItems": 1.5}),
                "$.maxItems must be a whole number of at least 0",
            ),
            (
                json!({"uniqueItems": 1}),
                "$.uniqueItems must be true or false",
            ),
            (
                json!({"minItems": 3, "maxItems": 2}),
                "$.minItems (3) is not below $.maxItems (2), so no value could match",
            ),
        ] {
            assert_eq!(refused(bad), expected);
        }
    }

    #[test]
    fn exclusive_bounds_leave_out_the_bound_itself() {
        let schema = json!({"exclusiveMinimum": 0, "exclusiveMaximum": 1});
        assert!(errors(schema.clone(), json!(0.5)).is_empty());
        assert_eq!(
            errors(schema.clone(), json!(0)),
            ["$: 0 must be more than 0"]
        );
        assert_eq!(
            errors(schema.clone(), json!(1)),
            ["$: 1 must be less than 1"]
        );
        // Only numbers: a string is not constrained.
        assert!(errors(schema, json!("x")).is_empty());
        for (bad, expected) in [
            (
                json!({"exclusiveMinimum": true}),
                "$.exclusiveMinimum must be a number",
            ),
            (
                json!({"exclusiveMinimum": 1, "exclusiveMaximum": 1}),
                "$.exclusiveMinimum (1) is not below $.exclusiveMaximum (1), so no value could \
                 match",
            ),
            (
                json!({"minimum": 2, "maximum": 1}),
                "$.minimum (2) is not below $.maximum (1), so no value could match",
            ),
            (
                json!({"minimum": 1, "exclusiveMaximum": 1}),
                "$.minimum (1) is not below $.exclusiveMaximum (1), so no value could match",
            ),
            (
                json!({"minLength": 2, "maxLength": 1}),
                "$.minLength (2) is not below $.maxLength (1), so no value could match",
            ),
        ] {
            assert_eq!(refused(bad), expected);
        }
        // Equal inclusive bounds allow exactly one value.
        assert!(errors(json!({"minimum": 1, "maximum": 1}), json!(1)).is_empty());
    }

    #[test]
    fn any_of_needs_one_match_and_shows_the_closest_alternative() {
        let schema = json!({"anyOf": [
            {"type": "string", "minLength": 2, "maxLength": 3},
            {"type": "object", "required": ["a", "b", "c"]}
        ]});
        assert!(errors(schema.clone(), json!("ab")).is_empty());
        assert!(errors(schema.clone(), json!({"a": 1, "b": 2, "c": 3})).is_empty());
        assert_eq!(
            errors(schema.clone(), json!("abcd")),
            [
                "$: matches none of the 2 alternatives in anyOf (closest, anyOf[0]: \
              $: 4 character(s), more than the maximum 3)"
            ]
        );
        // The closest is of the value's type, then has the fewest errors;
        // past two, the rest are counted.
        assert_eq!(
            errors(schema.clone(), json!({})),
            [
                "$: matches none of the 2 alternatives in anyOf (closest, anyOf[1]: \
              $: missing the required field \"a\"; $: missing the required field \"b\"; \
              and more)"
            ]
        );
        assert_eq!(
            errors(schema.clone(), json!(1)),
            [
                "$: matches none of the 2 alternatives in anyOf (closest, anyOf[0]: \
              $: expected string, got number)"
            ]
        );
        assert_eq!(
            errors(json!({"anyOf": [{"required": ["a", "b", "c"]}]}), json!({})),
            [
                "$: matches none of the 1 alternative in anyOf (closest, anyOf[0]: \
              $: missing the required field \"a\"; $: missing the required field \"b\"; \
              and more)"
            ]
        );
        // Nested: the place names the field.
        assert_eq!(
            errors(
                json!({"properties": {"x": {"anyOf": [{"type": "null"}, {"type": "integer"}]}}}),
                json!({"x": "1"})
            ),
            [
                "$.x: matches none of the 2 alternatives in anyOf (closest, anyOf[0]: \
              $.x: expected null, got string)"
            ]
        );
        for (bad, expected) in [
            (
                json!({"anyOf": []}),
                "$.anyOf must be a non-empty array of schemas",
            ),
            (
                json!({"anyOf": {}}),
                "$.anyOf must be a non-empty array of schemas",
            ),
            (
                json!({"anyOf": [{"pattern": 1}]}),
                "$.anyOf[0].pattern must be a string",
            ),
            (
                json!({"anyOf": [3]}),
                "$.anyOf[0] must be a schema (an object, or true or false), not 3",
            ),
        ] {
            assert_eq!(refused(bad), expected);
        }
    }

    #[test]
    fn one_of_needs_exactly_one_match() {
        let schema = json!({"oneOf": [
            {"type": "integer"},
            {"type": "number", "minimum": 10},
            {"type": "string"}
        ]});
        assert!(errors(schema.clone(), json!(3)).is_empty());
        assert!(errors(schema.clone(), json!(10.5)).is_empty());
        assert!(errors(schema.clone(), json!("x")).is_empty());
        assert_eq!(
            errors(schema.clone(), json!(12)),
            [
                "$: matches 2 of the 3 alternatives in oneOf (oneOf[0], oneOf[1]); exactly one \
              must match"
            ]
        );
        assert_eq!(
            errors(schema.clone(), json!(true)),
            [
                "$: matches none of the 3 alternatives in oneOf (closest, oneOf[0]: \
              $: expected integer, got boolean)"
            ]
        );
        assert_eq!(
            refused(json!({"oneOf": []})),
            "$.oneOf must be a non-empty array of schemas"
        );
        assert_eq!(
            refused(json!({"oneOf": [{"type": "text"}]})),
            "$.oneOf[0].type must be one of object, array, string, number, integer, boolean, \
             null, or a non-empty list of them, not \"text\""
        );
    }

    #[test]
    fn pattern_searches_the_string_anywhere_unless_anchored() {
        let schema = json!({"type": "string", "pattern": "[0-9]{2}"});
        assert!(errors(schema.clone(), json!("ab12cd")).is_empty());
        assert_eq!(
            errors(schema.clone(), json!("a1b")),
            ["$: \"a1b\" does not match the pattern \"[0-9]{2}\""]
        );
        let anchored = json!({"pattern": "^v[0-9]+$"});
        assert!(errors(anchored.clone(), json!("v12")).is_empty());
        assert_eq!(
            errors(anchored.clone(), json!("xv12")),
            ["$: \"xv12\" does not match the pattern \"^v[0-9]+$\""]
        );
        // Only strings: a number is not constrained.
        assert!(errors(anchored, json!(12)).is_empty());
        // Unicode: characters, not bytes.
        assert!(errors(json!({"pattern": "^é.$"}), json!("éß")).is_empty());
        assert_eq!(refused(json!({"pattern": 3})), "$.pattern must be a string");
        assert_eq!(
            refused(json!({"properties": {"id": {"pattern": "(ab"}}})),
            "$.properties.id.pattern is not a valid regular expression: unclosed group"
        );
    }

    #[test]
    fn annotations_are_allowed_unchecked_and_must_be_well_formed() {
        let annotated = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://example.com/r",
            "$comment": "for the reviewer",
            "title": "Result",
            "description": "what was found",
            "default": {"x": 1},
            "examples": [{"x": 2}],
            "deprecated": false,
            "readOnly": true,
            "writeOnly": false,
            "type": "object"
        });
        assert!(errors(annotated.clone(), json!({"y": "anything"})).is_empty());
        JsonSchema::declared(annotated, Dialect::Contract).unwrap();
        for (bad, expected) in [
            (json!({"title": 1}), "$.title must be a string"),
            (json!({"$comment": []}), "$.$comment must be a string"),
            (
                json!({"examples": 1}),
                "$.examples must be an array of values",
            ),
            (
                json!({"deprecated": "yes"}),
                "$.deprecated must be true or false",
            ),
            (json!({"readOnly": 0}), "$.readOnly must be true or false"),
            (
                json!({"writeOnly": null}),
                "$.writeOnly must be true or false",
            ),
        ] {
            assert_eq!(refused(bad), expected);
        }
        // `format` is an annotation in a map's schema, refused in a contract.
        assert!(errors(json!({"format": "email"}), json!("not an email")).is_empty());
        assert_eq!(
            contract_refused(json!({"properties": {"to": {"format": "email"}}})),
            "result.schema.properties.to uses `format`, which the contract does not support"
        );
    }

    #[test]
    fn keywords_outside_the_subset_are_refused_when_loaded() {
        for (bad, expected) in [
            (
                json!({"properties": {"a": {"$ref": "#/x"}}}),
                "$.properties.a uses `$ref`, which a map's schema does not support",
            ),
            (
                json!({"allOf": [{}]}),
                "$ uses `allOf`, which a map's schema does not support",
            ),
            (
                json!({"items": {"not": {}}}),
                "$.items uses `not`, which a map's schema does not support",
            ),
            (
                json!({"if": {}, "then": {}}),
                "$ uses `if`, which a map's schema does not support",
            ),
            (
                json!({"patternProperties": {}}),
                "$ uses `patternProperties`, which a map's schema does not support",
            ),
            (
                json!({"type": "text"}),
                "$.type must be one of object, array, string, number, integer, boolean, null, \
                 or a non-empty list of them, not \"text\"",
            ),
            (
                json!({"type": []}),
                "$.type must be one of object, array, string, number, integer, boolean, null, \
                 or a non-empty list of them, not []",
            ),
            (
                json!({"type": ["string", "string"]}),
                "$.type names \"string\" twice (at 1)",
            ),
            (
                json!({"minLength": -1}),
                "$.minLength must be a whole number of at least 0",
            ),
            (
                json!({"required": "a"}),
                "$.required must be an array of property names",
            ),
            (
                json!({"items": [{}]}),
                "$.items must be one schema; a list of schemas (prefixItems) is not supported",
            ),
            (
                json!({"enum": []}),
                "$.enum must be a non-empty array of values",
            ),
            (
                json!({"required": ["b"], "additionalProperties": false, "properties": {"a": {}}}),
                "$.required names `b`, which is not in properties while additionalProperties \
                 is false, so no value could match",
            ),
            (
                json!(3),
                "$ must be a schema (an object, or true or false), not 3",
            ),
        ] {
            assert_eq!(refused(bad), expected);
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

    #[test]
    fn errors_are_capped_at_twenty() {
        let list: Vec<u64> = (0..30).collect();
        let found = errors(json!({"items": {"type": "string"}}), json!(list));
        assert_eq!(found.len(), ERRORS_MAX);
        assert_eq!(found[0], "$[0]: expected string, got number");
    }
}
