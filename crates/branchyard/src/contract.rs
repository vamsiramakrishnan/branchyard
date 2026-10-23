//! A result contract: the shape a child's answer must take, said by its
//! parent at spawn and read back as JSON.
//!
//! A parent that reads its children's answers as prose misreads them. At
//! `spawn` it can give the child a [`ResultContract`]: a JSON Schema in a
//! small subset and whether the child must answer. The child answers by
//! calling `report` (or `by report`) with a `result`, a JSON value. With a
//! contract the value is checked before anything is recorded, and a value
//! that does not match is refused naming where and why
//! (`result.files[2]: expected string, got number`), so the child can fix
//! it and report again. An accepted result is recorded on the child as
//! [`Activity::Result`], and the latest one is the child's
//! [`Inspection::result`](crate::Inspection::result), which `inspect` and
//! `wait` carry.
//!
//! The subset is the one `by map` checks ([`crate::JsonSchema`], one
//! checker): the keywords [`KEYWORDS`] (`type`, `properties`, `required`,
//! `additionalProperties`, `items`, `enum`, `const`, `minimum`, `maximum`,
//! `exclusiveMinimum`, `exclusiveMaximum`, `minLength`, `maxLength`,
//! `pattern`, `minItems`, `maxItems`, `uniqueItems`, `anyOf`, `oneOf`) and
//! the unchecked annotations [`ANNOTATIONS`]. Anything else (`$ref`,
//! `allOf`, `not`, `if`, `format`, ...) is refused when the contract is
//! given, by name, so no rule a schema states is left unchecked; so is a
//! keyword in the wrong form, an invalid `pattern`, bounds no value could
//! meet, and a schema of `false`. Keywords apply as in JSON Schema:
//! `properties` constrains an object and says nothing about a string, so
//! declare `type` for the shape you want.
//!
//! A JSON `null` reported as a result is the same as none (on the wire,
//! `"result": null` and no `result` read alike to most clients), except
//! under a contract whose schema accepts `null` (`type: "null"`, a list of
//! types with `null`, an `anyOf` with it, ...): there it is a result,
//! recorded and read back as `null`.
//!
//! Not done: a result is not checked against anything but the schema (that
//! it is true is the child's claim, like a self-reported status); and
//! nothing here stops a child from ending its turn without a result: a
//! `required` contract only records a warning on the parent when it does
//! (the `settled` check at the end of a turn).

use branchyard_support::best_effort;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::json_schema::{Dialect, JsonSchema};
use crate::record::{self, Recorder};
use crate::{Activity, BranchStatus, Error, Observer, RecordedEvent, Yard};

/// The keywords a contract's schema checks: the same as a map's schema
/// (`docs/map.md#schemas`).
pub const KEYWORDS: &[&str] = crate::json_schema::KEYWORDS;

/// The annotations a contract's schema may carry, unchecked. Unlike a
/// map's schema, a contract's refuses `format`: it would read as a rule
/// nobody checks.
pub const ANNOTATIONS: &[&str] = crate::json_schema::ANNOTATIONS;

/// The names `type` may take.
pub const TYPES: &[&str] = crate::json_schema::TYPES;

/// The longest contract schema, as compact JSON, in bytes.
pub const SCHEMA_MAX: usize = 16 * 1024;

/// The longest result, as compact JSON, in bytes.
pub const RESULT_MAX: usize = 64 * 1024;

/// Most violations one refusal names; the rest are counted.
const VIOLATIONS_SHOWN: usize = 8;

/// What a parent says, at spawn, about the answer it wants from a child.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultContract {
    /// A JSON Schema in the subset the contract supports: `type`,
    /// `properties`, `required`, `additionalProperties`, `items`, `enum`,
    /// `const`, `minimum`, `maximum`, `exclusiveMinimum`,
    /// `exclusiveMaximum`, `minLength`, `maxLength`, `pattern`,
    /// `minItems`, `maxItems`, `uniqueItems`, `anyOf` and `oneOf`, with the
    /// annotations `title`, `description`, `$comment`, `default`,
    /// `examples`, `deprecated`, `readOnly`, `writeOnly`, `$schema` and
    /// `$id`. A result reported with `report` must match it.
    pub schema: Value,
    /// Whether the child must report a result: a `report` without one is
    /// refused, and a child whose turn ends with none leaves a warning in
    /// its parent's log. Default false.
    #[serde(default)]
    pub required: bool,
}

impl ResultContract {
    /// A contract for `schema`, or why the schema is not in the subset.
    pub fn new(schema: Value, required: bool) -> Result<ResultContract, Error> {
        let contract = ResultContract { schema, required };
        contract.check()?;
        Ok(contract)
    }

    /// Whether the schema is in the subset: the refusal names the place
    /// and the keyword (``result.schema uses `pattern`, which the contract
    /// does not support``).
    pub fn check(&self) -> Result<(), Error> {
        check_schema(&self.schema).map_err(Error::Denied)
    }

    /// Every way `result` fails the schema (at most 20), each as
    /// `result.files[2]: expected string, got number`. Empty when it
    /// matches.
    pub fn violations(&self, result: &Value) -> Result<Vec<String>, Error> {
        Ok(self.compiled()?.validate_at(result, "result"))
    }

    /// Whether a JSON `null` matches the schema, so that `null` is a
    /// result under this contract rather than none.
    pub fn accepts_null(&self) -> bool {
        self.compiled()
            .is_ok_and(|schema| schema.accepts(&Value::Null))
    }

    fn compiled(&self) -> Result<JsonSchema, Error> {
        JsonSchema::declared(self.schema.clone(), Dialect::Contract)
            .map_err(|why| Error::State(format!("the stored result contract is invalid: {why}")))
    }

    /// `result` if it matches the schema, else a refusal naming each place
    /// it does not (the first eight).
    pub fn validate(&self, result: &Value) -> Result<(), Error> {
        let violations = self.violations(result)?;
        if violations.is_empty() {
            return Ok(());
        }
        let mut text = violations
            .iter()
            .take(VIOLATIONS_SHOWN)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        if violations.len() > VIOLATIONS_SHOWN {
            text.push_str("; and more");
        }
        Err(Error::Denied(text))
    }
}

/// Check `schema` against the subset, with the refusal naming the place
/// (`result.schema.properties.name`) and the rule.
pub fn check_schema(schema: &Value) -> Result<(), String> {
    let size = schema.to_string().len();
    if size > SCHEMA_MAX {
        return Err(format!(
            "result.schema is {size} bytes; keep it to at most {SCHEMA_MAX}"
        ));
    }
    JsonSchema::declared(schema.clone(), Dialect::Contract)?;
    if schema == &Value::Bool(false) {
        return Err("result.schema is false, so the contract could never be met".into());
    }
    Ok(())
}

/// The result `report` may record: `result` if it matches the branch's
/// `contract` (or it has none), `None` when none was given, or the
/// refusal. A branch whose contract is `required` may not report without
/// one, and no result may be larger than [`RESULT_MAX`] bytes. A JSON
/// `null` is the same as none, unless the contract's schema accepts
/// `null` ([`ResultContract::accepts_null`]): then it is the result.
pub fn accept(
    contract: Option<&ResultContract>,
    result: Option<Value>,
) -> Result<Option<Value>, Error> {
    let null_is_a_result = contract.is_some_and(ResultContract::accepts_null);
    let result = result.filter(|value| !value.is_null() || null_is_a_result);
    let Some(value) = result else {
        return match contract {
            Some(contract) if contract.required => Err(Error::Denied(
                "this branch's result contract requires a result: report with a `result` that \
                 matches the `result_contract` in your inspect"
                    .into(),
            )),
            _ => Ok(None),
        };
    };
    let size = value.to_string().len();
    if size > RESULT_MAX {
        return Err(Error::Denied(format!(
            "result is {size} bytes; keep it to at most {RESULT_MAX}"
        )));
    }
    if let Some(contract) = contract {
        contract.validate(&value)?;
    }
    Ok(Some(value))
}

/// For `#[serde(default, deserialize_with = "present")]` on an
/// `Option<Value>` result: a field that is present is `Some`, even when it
/// is JSON `null`, so a `null` result is not lost on the wire (serde reads
/// a present `null` as `None` otherwise). An absent field is still `None`.
pub fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// The latest result in `events`, oldest first, if the branch ever
/// reported one.
pub fn latest(events: &[RecordedEvent]) -> Option<Value> {
    events.iter().rev().find_map(|event| match &event.activity {
        Activity::Result { value } => Some(value.clone()),
        _ => None,
    })
}

/// Whether `events` hold a result accepted after the last prompt: during
/// the turn that prompt started.
fn reported_this_turn(events: &[RecordedEvent]) -> bool {
    events
        .iter()
        .rev()
        .take_while(|event| !matches!(event.activity, Activity::Prompt(_)))
        .any(|event| matches!(event.activity, Activity::Result { .. }))
}

/// A result as one line: compact JSON, cut to `max` characters with an
/// ellipsis.
pub fn one_line(value: &Value, max: usize) -> String {
    let text = value.to_string();
    match text.chars().count() > max {
        true => format!("{}…", text.chars().take(max).collect::<String>()),
        false => text,
    }
}

/// What a parent's log says when `name`, whose `contract` requires a
/// result, ended a turn with `status` and `events` (its log) holding none
/// accepted during that turn; `None` when nothing is missing. Only a turn
/// that ended as finished work (ready, no changes or merged) counts: one
/// that failed, was interrupted or ran out of budget says why in its status.
fn missing_warning(
    name: &str,
    contract: &ResultContract,
    status: &BranchStatus,
    events: &[RecordedEvent],
) -> Option<String> {
    let finished = matches!(
        status,
        BranchStatus::Ready | BranchStatus::NoChanges | BranchStatus::Merged { .. }
    );
    if !contract.required || !finished || reported_this_turn(events) {
        return None;
    }
    let earlier = match latest(events) {
        Some(_) => "its result is the one from an earlier turn",
        None => "it reported none",
    };
    Some(format!(
        "{name} ended its turn without the result its contract requires: {earlier}. Ask it for \
         one with `by send {name} \"...\"`, or read what it did with `by inspect {name}`"
    ))
}

/// After a delegated child's turn: when its contract requires a result and
/// the turn ended with none accepted during it, record a warning in the
/// parent's log naming the child ([`missing_warning`]).
pub(crate) fn settled(yard: &Yard, name: &str, observer: Option<Observer>) {
    let store = yard.store();
    let Ok(record) = store.read(name) else {
        return;
    };
    let (Some(parent), Some(contract)) = (&record.info.parent, &record.result_contract) else {
        return;
    };
    if !contract.required {
        return;
    }
    let Some(events) = best_effort(
        "read a child's events for its result contract",
        record::read(&store, name),
    ) else {
        return;
    };
    let Some(warning) = missing_warning(name, contract, &record.info.status, &events) else {
        return;
    };
    best_effort(
        "warn a parent that a child's required result is missing",
        Recorder::open(&store, parent, observer)
            .and_then(|mut recorder| recorder.record(Activity::Warning(warning))),
    );
}

/// The line a wake prompt shows for the child `name`: its latest result, cut
/// to `max` characters, or, when its contract requires one and it reported
/// none, a warning saying so; `None` when there is nothing to say.
pub(crate) fn wake_line(
    name: &str,
    contract: Option<&ResultContract>,
    result: Option<&Value>,
    max: usize,
) -> Option<String> {
    match (result, contract) {
        (Some(result), _) => Some(format!(
            "result: {} (all of it: `by inspect {name}`)",
            one_line(result, max)
        )),
        (None, Some(contract)) if contract.required => {
            Some("warning: its result contract requires a result, and it reported none".to_owned())
        }
        (None, _) => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn contract(schema: Value) -> ResultContract {
        ResultContract::new(schema, false).unwrap()
    }

    fn refused(schema: Value) -> String {
        match ResultContract::new(schema, false) {
            Err(Error::Denied(message)) => message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn mismatch(contract: &ResultContract, value: Value) -> String {
        match contract.validate(&value) {
            Err(Error::Denied(message)) => message,
            other => panic!("expected a mismatch, got {other:?}"),
        }
    }

    /// The contract the other tests build on: every supported keyword.
    fn files() -> ResultContract {
        contract(json!({
            "type": "object",
            "description": "what the child found",
            "required": ["files", "verdict"],
            "additionalProperties": false,
            "properties": {
                "files": {"type": "array", "items": {"type": "string", "minLength": 1}},
                "verdict": {"enum": ["pass", "fail"]},
                "score": {"type": "number", "minimum": 0, "maximum": 1},
                "count": {"type": "integer"},
                "ok": {"type": "boolean"},
                "note": {"type": "string", "maxLength": 5},
                "extra": {"type": "null"}
            }
        }))
    }

    #[test]
    fn a_matching_result_is_accepted() {
        let files = files();
        files
            .validate(&json!({"files": ["a.rs", "b.rs"], "verdict": "pass"}))
            .unwrap();
        files
            .validate(&json!({
                "files": [], "verdict": "fail", "score": 0.5, "count": 3, "ok": true,
                "note": "short", "extra": null
            }))
            .unwrap();
        // An integer written as 3.0 is an integer, as in JSON Schema.
        files
            .validate(&json!({"files": [], "verdict": "pass", "count": 3.0}))
            .unwrap();
    }

    #[test]
    fn each_keyword_refuses_naming_the_path_and_the_rule() {
        let files = files();
        let valid = json!({"files": ["a"], "verdict": "pass"});
        let with = |field: &str, value: Value| {
            let mut result = valid.clone();
            result[field] = value;
            mismatch(&files, result)
        };
        assert_eq!(
            mismatch(&files, json!({"files": ["a", "b", 3], "verdict": "pass"})),
            "result.files[2]: expected string, got number"
        );
        assert_eq!(
            mismatch(&files, json!([1])),
            "result: expected object, got array"
        );
        assert_eq!(
            mismatch(&files, json!({"files": ["a"]})),
            "result: missing the required field \"verdict\""
        );
        assert_eq!(
            with("surprise", json!(1)),
            "result: the field \"surprise\" is not allowed"
        );
        assert_eq!(
            with("verdict", json!("maybe")),
            "result.verdict: \"maybe\" is not one of \"pass\", \"fail\""
        );
        assert_eq!(
            with("score", json!(-1)),
            "result.score: -1 is less than the minimum 0"
        );
        assert_eq!(
            with("score", json!(1.5)),
            "result.score: 1.5 is more than the maximum 1"
        );
        assert_eq!(
            with("count", json!(1.5)),
            "result.count: expected integer, got number"
        );
        assert_eq!(
            with("ok", json!("yes")),
            "result.ok: expected boolean, got string"
        );
        assert_eq!(
            with("extra", json!(0)),
            "result.extra: expected null, got number"
        );
        assert_eq!(
            with("files", json!([""])),
            "result.files[0]: 0 character(s), fewer than the minimum 1"
        );
        assert_eq!(
            with("note", json!("too long")),
            "result.note: 8 character(s), more than the maximum 5"
        );
        assert_eq!(
            with("files", json!("a")),
            "result.files: expected array, got string"
        );
    }

    #[test]
    fn nested_schemas_name_the_whole_path() {
        let nested = contract(json!({
            "type": "object",
            "properties": {
                "findings": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "required": ["file", "line"],
                        "properties": {
                            "file": {"type": "string"},
                            "line": {"type": "integer", "minimum": 1},
                            "tags": {"type": "array", "items": {"enum": ["bug", "style"]}}
                        }
                    }
                }
            }
        }));
        nested
            .validate(&json!({"findings": [{"file": "a.rs", "line": 4, "tags": ["bug"]}]}))
            .unwrap();
        assert_eq!(
            mismatch(
                &nested,
                json!({"findings": [{"file": "a.rs", "line": 4}, {"file": "b.rs", "line": 0}]})
            ),
            "result.findings[1].line: 0 is less than the minimum 1"
        );
        assert_eq!(
            mismatch(
                &nested,
                json!({"findings": [{"file": "a.rs", "line": 1, "tags": ["bug", "nit"]}]})
            ),
            "result.findings[0].tags[1]: \"nit\" is not one of \"bug\", \"style\""
        );
        assert_eq!(
            mismatch(&nested, json!({"findings": [{"file": 7}]})),
            "result.findings[0]: missing the required field \"line\"; \
             result.findings[0].file: expected string, got number"
        );
    }

    #[test]
    fn many_violations_are_named_up_to_eight() {
        let strings = contract(json!({"type": "array", "items": {"type": "string"}}));
        let message = mismatch(&strings, json!([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]));
        assert_eq!(message.matches("expected string").count(), 8);
        assert!(message.starts_with("result[0]: expected string, got number"));
        assert!(message.ends_with("; and more"), "{message}");
    }

    #[test]
    fn a_keyword_outside_the_subset_is_refused_by_name_wherever_it_is() {
        assert_eq!(
            refused(json!({"type": "string", "$ref": "#/a"})),
            "result.schema uses `$ref`, which the contract does not support"
        );
        assert_eq!(
            refused(json!({"properties": {"name": {"allOf": []}}})),
            "result.schema.properties.name uses `allOf`, which the contract does not support"
        );
        assert_eq!(
            refused(json!({"type": "array", "items": {"type": "array", "items": {"not": {}}}})),
            "result.schema.items.items uses `not`, which the contract does not support"
        );
        assert_eq!(
            refused(json!({"anyOf": [{"if": {}}]})),
            "result.schema.anyOf[0] uses `if`, which the contract does not support"
        );
        // `format` is an annotation to `by map`, and refused here: nothing
        // would check it.
        for keyword in [
            "format",
            "patternProperties",
            "prefixItems",
            "dependentRequired",
        ] {
            let message = refused(json!({"type": "object", keyword: "x"}));
            assert_eq!(
                message,
                format!("result.schema uses `{keyword}`, which the contract does not support")
            );
        }
    }

    #[test]
    fn the_richer_keywords_are_checked_with_readable_errors() {
        let rich = contract(json!({
            "title": "Review",
            "$comment": "checked by the parent",
            "type": "object",
            "required": ["kind", "id", "files", "score"],
            "properties": {
                "kind": {"const": "review"},
                "id": {"type": "string", "pattern": "^[A-Z]+-[0-9]+$", "examples": ["BY-12"]},
                "files": {"type": "array", "items": {"type": "string"}, "minItems": 1,
                          "maxItems": 3, "uniqueItems": true},
                "score": {"exclusiveMinimum": 0, "exclusiveMaximum": 1, "default": 0.5},
                "verdict": {"anyOf": [{"enum": ["pass", "fail"]}, {"type": "null"}]},
                "owner": {"oneOf": [{"type": "string"}, {"type": "object", "required": ["team"]}]}
            }
        }));
        let good = json!({"kind": "review", "id": "BY-12", "files": ["a.rs"], "score": 0.5,
                          "verdict": null, "owner": {"team": "x"}});
        rich.validate(&good).unwrap();
        let with = |field: &str, value: Value| {
            let mut result = good.clone();
            result[field] = value;
            mismatch(&rich, result)
        };
        assert_eq!(
            with("kind", json!("audit")),
            "result.kind: must be \"review\""
        );
        assert_eq!(
            with("id", json!("by-12")),
            "result.id: \"by-12\" does not match the pattern \"^[A-Z]+-[0-9]+$\""
        );
        assert_eq!(
            with("files", json!([])),
            "result.files: 0 item(s), fewer than the minimum 1"
        );
        assert_eq!(
            with("files", json!(["a", "b", "c", "d"])),
            "result.files: 4 item(s), more than the maximum 3"
        );
        assert_eq!(
            with("files", json!(["a", "a"])),
            "result.files[1]: repeats the item at [0]"
        );
        assert_eq!(
            with("score", json!(1)),
            "result.score: 1 must be less than 1"
        );
        assert_eq!(
            with("score", json!(0)),
            "result.score: 0 must be more than 0"
        );
        assert_eq!(
            with("verdict", json!("maybe")),
            "result.verdict: matches none of the 2 alternatives in anyOf (closest, anyOf[0]: \
             result.verdict: \"maybe\" is not one of \"pass\", \"fail\")"
        );
        assert_eq!(
            with("owner", json!(3)),
            "result.owner: matches none of the 2 alternatives in oneOf (closest, oneOf[0]: \
             result.owner: expected string, got number)"
        );
        // Declared wrong, each is refused naming its place.
        for (schema, expected) in [
            (
                json!({"properties": {"id": {"pattern": "[a-"}}}),
                "result.schema.properties.id.pattern is not a valid regular expression: \
                 unclosed character class",
            ),
            (
                json!({"minItems": 2, "maxItems": 1}),
                "result.schema.minItems (2) is not below result.schema.maxItems (1), so no \
                 value could match",
            ),
            (
                json!({"oneOf": []}),
                "result.schema.oneOf must be a non-empty array of schemas",
            ),
            (
                json!({"uniqueItems": "yes"}),
                "result.schema.uniqueItems must be true or false",
            ),
            (json!({"title": 1}), "result.schema.title must be a string"),
        ] {
            assert_eq!(refused(schema.clone()), expected, "{schema}");
        }
    }

    #[test]
    fn a_malformed_keyword_is_refused_saying_what_it_needs() {
        for (schema, expected) in [
            (
                json!({"type": "text"}),
                "result.schema.type must be one of object, array, string, number, integer, \
                 boolean, null, or a non-empty list of them, not \"text\"",
            ),
            (
                json!({"type": ["string", "nil"]}),
                "result.schema.type must be one of object, array, string, number, integer, \
                 boolean, null, or a non-empty list of them, not [\"string\",\"nil\"]",
            ),
            (
                json!({"properties": []}),
                "result.schema.properties must be an object of schemas",
            ),
            (
                json!({"properties": {"a": 1}}),
                "result.schema.properties.a must be a schema (an object, or true or false), not 1",
            ),
            (
                json!({"required": "a"}),
                "result.schema.required must be an array of property names",
            ),
            (
                json!({"required": ["a", 1]}),
                "result.schema.required must be an array of property names",
            ),
            (
                json!({"additionalProperties": 1}),
                "result.schema.additionalProperties must be a schema (an object, or true or \
                 false), not 1",
            ),
            (
                json!({"items": [{"type": "string"}]}),
                "result.schema.items must be one schema; a list of schemas (prefixItems) is not \
                 supported",
            ),
            (
                json!({"enum": []}),
                "result.schema.enum must be a non-empty array of values",
            ),
            (
                json!({"enum": "a"}),
                "result.schema.enum must be a non-empty array of values",
            ),
            (
                json!({"minimum": "1"}),
                "result.schema.minimum must be a number",
            ),
            (
                json!({"maximum": null}),
                "result.schema.maximum must be a number",
            ),
            (
                json!({"minLength": -1}),
                "result.schema.minLength must be a whole number of at least 0",
            ),
            (
                json!({"maxLength": 1.5}),
                "result.schema.maxLength must be a whole number of at least 0",
            ),
            (
                json!({"description": 3}),
                "result.schema.description must be a string",
            ),
            (
                json!("string"),
                "result.schema must be a schema (an object, or true or false), not \"string\"",
            ),
            (
                json!(false),
                "result.schema is false, so the contract could never be met",
            ),
        ] {
            assert_eq!(refused(schema.clone()), expected, "{schema}");
        }
        // A list of types, a schema for extra properties and boolean
        // schemas are in the subset.
        let open = contract(json!({
            "type": ["object", "null"],
            "properties": {"a": true, "b": false},
            "additionalProperties": {"type": "string"}
        }));
        open.validate(&json!({"a": [1], "c": "x"})).unwrap();
        assert_eq!(
            mismatch(&open, json!({"b": 1, "c": 2})),
            "result.b: no value is allowed here; result.c: expected string, got number"
        );
        contract(json!(true)).validate(&json!(1)).unwrap();
    }

    #[test]
    fn a_required_property_the_schema_forbids_is_refused() {
        assert_eq!(
            refused(json!({
                "required": ["a", "b"],
                "additionalProperties": false,
                "properties": {"a": {"type": "string"}}
            })),
            "result.schema.required names `b`, which is not in properties while \
             additionalProperties is false, so no value could match"
        );
        // With extra properties allowed, a required one need not be listed.
        contract(json!({"required": ["b"], "properties": {"a": {}}}));
        contract(json!({"required": ["b"], "additionalProperties": true}));
    }

    #[test]
    fn null_is_a_result_only_under_a_contract_whose_schema_accepts_it() {
        for schema in [
            json!({"type": "null"}),
            json!({"type": ["object", "null"]}),
            json!({"anyOf": [{"type": "object"}, {"const": null}]}),
            json!({"enum": [null, 1]}),
            json!({}),
        ] {
            for required in [false, true] {
                let wanted = ResultContract::new(schema.clone(), required).unwrap();
                assert!(wanted.accepts_null(), "{schema}");
                assert_eq!(
                    accept(Some(&wanted), Some(Value::Null)).unwrap(),
                    Some(Value::Null),
                    "{schema}"
                );
                // No result at all is still none.
                assert_eq!(accept(Some(&wanted), None).is_err(), required);
            }
        }
        // A schema that refuses null: null is none, as without a contract.
        let object = ResultContract::new(json!({"type": "object"}), false).unwrap();
        assert!(!object.accepts_null());
        assert_eq!(accept(Some(&object), Some(Value::Null)).unwrap(), None);
        assert_eq!(accept(None, Some(Value::Null)).unwrap(), None);
    }

    #[test]
    fn a_present_null_reads_as_a_result_and_an_absent_field_as_none() {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default, deserialize_with = "present")]
            result: Option<Value>,
        }
        let read = |text: &str| serde_json::from_str::<Wire>(text).unwrap().result;
        assert_eq!(read(r#"{"result": null}"#), Some(Value::Null));
        assert_eq!(read(r#"{"result": [1]}"#), Some(json!([1])));
        assert_eq!(read("{}"), None);
    }

    #[test]
    fn a_schema_over_the_size_limit_is_refused() {
        let big = json!({"enum": vec!["x".repeat(100); 200]});
        let message = refused(big);
        assert!(message.starts_with("result.schema is "), "{message}");
        assert!(message.ends_with("keep it to at most 16384"), "{message}");
    }

    #[test]
    fn a_result_is_accepted_by_the_contract_or_refused_before_anything_is_recorded() {
        let contract = ResultContract::new(json!({"type": "object"}), true).unwrap();
        let ok = json!({"a": 1});
        assert_eq!(
            accept(Some(&contract), Some(ok.clone())).unwrap(),
            Some(ok.clone())
        );
        assert!(accept(Some(&contract), Some(json!("text"))).is_err());
        // Required: none is refused, null counts as none.
        for none in [None, Some(Value::Null)] {
            let Err(Error::Denied(message)) = accept(Some(&contract), none) else {
                panic!("a required result was not demanded");
            };
            assert!(message.contains("requires a result"), "{message}");
            assert!(message.contains("result_contract"), "{message}");
        }
        // Not required: none is fine.
        let optional = ResultContract::new(json!({"type": "object"}), false).unwrap();
        assert_eq!(accept(Some(&optional), None).unwrap(), None);
        // Without a contract, whatever is given is accepted as is.
        assert_eq!(
            accept(None, Some(json!([1, "a"]))).unwrap(),
            Some(json!([1, "a"]))
        );
        assert_eq!(accept(None, None).unwrap(), None);
    }

    #[test]
    fn a_result_over_the_size_limit_is_refused() {
        let big = json!("x".repeat(RESULT_MAX));
        let Err(Error::Denied(message)) = accept(None, Some(big)) else {
            panic!("a result over the limit was accepted");
        };
        assert!(message.starts_with("result is "), "{message}");
        assert!(message.ends_with("at most 65536"), "{message}");
    }

    #[test]
    fn the_latest_result_wins_and_a_prompt_ends_the_turn() {
        let event = |activity| RecordedEvent { at_ms: 1, activity };
        let result = |n: u64| event(Activity::Result { value: json!(n) });
        let events = vec![
            event(Activity::Prompt("first".into())),
            result(1),
            event(Activity::Warning("w".into())),
            result(2),
            event(Activity::Prompt("second".into())),
        ];
        assert_eq!(latest(&events), Some(json!(2)));
        assert_eq!(latest(&events[..2]), Some(json!(1)));
        assert_eq!(latest(&[]), None);
        // The second turn has not reported; the first did.
        assert!(!reported_this_turn(&events));
        assert!(reported_this_turn(&events[..4]));
        let mut later = events;
        later.push(result(3));
        assert!(reported_this_turn(&later));
        assert!(!reported_this_turn(&[]));
    }

    fn event(activity: Activity) -> RecordedEvent {
        RecordedEvent { at_ms: 1, activity }
    }

    #[test]
    fn a_required_result_left_out_warns_only_after_finished_work() {
        let required = ResultContract::new(json!({"type": "object"}), true).unwrap();
        let optional = ResultContract::new(json!({"type": "object"}), false).unwrap();
        let prompted = vec![event(Activity::Prompt("go".into()))];
        let warn = |contract: &ResultContract, status: &BranchStatus, events: &[RecordedEvent]| {
            missing_warning("kid", contract, status, events)
        };
        // Finished work with no result: a warning naming the child.
        for status in [
            BranchStatus::Ready,
            BranchStatus::NoChanges,
            BranchStatus::Merged {
                target: "main".into(),
                commit: "c".into(),
            },
        ] {
            let text = warn(&required, &status, &prompted).unwrap();
            assert!(
                text.starts_with(
                    "kid ended its turn without the result its contract requires: it reported none."
                ),
                "{text}"
            );
            assert!(text.contains("`by inspect kid`"), "{text}");
        }
        // Not required, or a turn that says why in its status: nothing.
        assert_eq!(warn(&optional, &BranchStatus::Ready, &prompted), None);
        for status in [
            BranchStatus::Interrupted,
            BranchStatus::Failed { reason: "x".into() },
            BranchStatus::BudgetExceeded {
                limit: "max_usd".into(),
            },
            BranchStatus::Running,
            BranchStatus::WaitingOnChildren,
        ] {
            assert_eq!(warn(&required, &status, &prompted), None, "{status:?}");
        }
        // A result accepted in this turn satisfies it.
        let mut reported = prompted.clone();
        reported.push(event(Activity::Result { value: json!({}) }));
        assert_eq!(warn(&required, &BranchStatus::Ready, &reported), None);
        // One from an earlier turn does not, and the warning says so.
        reported.push(event(Activity::Prompt("again".into())));
        let text = warn(&required, &BranchStatus::Ready, &reported).unwrap();
        assert!(
            text.contains("its result is the one from an earlier turn"),
            "{text}"
        );
    }

    #[test]
    fn a_wake_shows_a_childs_result_or_that_a_required_one_is_missing() {
        let required = ResultContract::new(json!({"type": "object"}), true).unwrap();
        let optional = ResultContract::new(json!({"type": "object"}), false).unwrap();
        let result = json!({"files": ["a.rs"]});
        assert_eq!(
            wake_line("kid", Some(&required), Some(&result), 100).unwrap(),
            "result: {\"files\":[\"a.rs\"]} (all of it: `by inspect kid`)"
        );
        assert_eq!(
            wake_line("kid", None, Some(&result), 100).unwrap(),
            "result: {\"files\":[\"a.rs\"]} (all of it: `by inspect kid`)"
        );
        assert!(wake_line("kid", Some(&required), None, 100)
            .unwrap()
            .starts_with("warning: its result contract requires a result"));
        assert_eq!(wake_line("kid", Some(&optional), None, 100), None);
        assert_eq!(wake_line("kid", None, None, 100), None);
        // A long result is cut, with an ellipsis.
        assert!(wake_line("kid", None, Some(&result), 7)
            .unwrap()
            .starts_with("result: {\"files…"));
    }

    #[test]
    fn a_result_shows_on_one_line_cut_to_a_length() {
        let value = json!({"files": ["a.rs"], "ok": true});
        assert_eq!(one_line(&value, 100), r#"{"files":["a.rs"],"ok":true}"#);
        assert_eq!(one_line(&value, 8), "{\"files\"…");
    }

    #[test]
    fn the_wire_forms_omit_nothing_and_default_required_to_false() {
        let contract = ResultContract::new(json!({"type": "string"}), true).unwrap();
        assert_eq!(
            serde_json::to_value(&contract).unwrap(),
            json!({"schema": {"type": "string"}, "required": true})
        );
        let read: ResultContract =
            serde_json::from_value(json!({"schema": {"type": "string"}})).unwrap();
        assert!(!read.required);
        assert!(
            serde_json::from_value::<ResultContract>(json!({"schema": {}, "requried": true}))
                .is_err()
        );
        let activity = Activity::Result {
            value: json!({"ok": true}),
        };
        assert_eq!(
            serde_json::to_value(&activity).unwrap(),
            json!({"result": {"value": {"ok": true}}})
        );
        assert_eq!(
            serde_json::from_value::<Activity>(json!({"result": {"value": [1]}})).unwrap(),
            Activity::Result { value: json!([1]) }
        );
    }
}
