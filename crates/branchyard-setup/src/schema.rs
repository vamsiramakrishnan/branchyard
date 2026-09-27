//! The published JSON Schemas: `schema/branchyard.config.json` for the
//! configuration file (editors complete it through the `#:schema` line of
//! a generated file) and `schema/setup.protocol.json` for the setup
//! protocol's documents. `tests/schema.rs` checks the checked-in files are
//! exactly what these print; regenerate with
//! `cargo run -p branchyard-setup --example generate_schemas`.

use schemars::{schema_for, JsonSchema, Schema};
use serde_json::{Map, Value};

use crate::config::ProjectConfig;
use crate::interview::Answers;
use crate::protocol::{Applied, ErrorResponse, Response, TopicList};

/// `schema/branchyard.config.json`.
pub fn config_json() -> String {
    let mut schema = schema_for!(ProjectConfig).to_value();
    if let Value::Object(map) = &mut schema {
        map.insert(
            "$id".into(),
            Value::String(crate::config::SCHEMA_URL.into()),
        );
    }
    pretty(&schema)
}

fn entry<T: JsonSchema>(name: &str, out: &mut Map<String, Value>) {
    let schema: Schema = schema_for!(T);
    out.insert(name.into(), schema.to_value());
}

/// `schema/setup.protocol.json`: `{"schema", "version", "types": {...}}`,
/// each type a self-contained schema document, like `schema/contract.json`.
pub fn protocol_json() -> String {
    let mut types = Map::new();
    entry::<Answers>("Answers", &mut types);
    entry::<Response>("Response", &mut types);
    entry::<Applied>("Applied", &mut types);
    entry::<TopicList>("TopicList", &mut types);
    entry::<ErrorResponse>("ErrorResponse", &mut types);
    let mut doc = Map::new();
    doc.insert(
        "schema".into(),
        Value::String("branchyard/setup-protocol/v1".into()),
    );
    doc.insert("protocol".into(), Value::String(crate::PROTOCOL.into()));
    doc.insert("version".into(), Value::from(1));
    doc.insert("types".into(), Value::Object(types));
    pretty(&Value::Object(doc))
}

fn pretty(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).expect("a schema serializes");
    text.push('\n');
    text
}
