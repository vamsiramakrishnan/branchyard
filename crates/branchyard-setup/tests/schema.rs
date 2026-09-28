//! `schema/branchyard.config.json` and `schema/setup.protocol.json` are
//! generated from the Rust types; a changed type without
//! `cargo run -p branchyard-setup --example generate_schemas` fails here.

use std::path::Path;

fn check(name: &str, generated: String) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../schema")
        .join(name);
    let checked_in = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        checked_in == generated,
        "schema/{name} is stale; regenerate with:\n  cargo run -p branchyard-setup --example generate_schemas"
    );
}

#[test]
fn the_configuration_schema_is_fresh() {
    check(
        "branchyard.config.json",
        branchyard_setup::schema::config_json(),
    );
}

#[test]
fn the_protocol_schema_is_fresh() {
    check(
        "setup.protocol.json",
        branchyard_setup::schema::protocol_json(),
    );
}

#[test]
fn the_configuration_schema_describes_every_table() {
    let schema: serde_json::Value =
        serde_json::from_str(&branchyard_setup::schema::config_json()).unwrap();
    let properties = schema["properties"].as_object().unwrap();
    for table in [
        "version",
        "defaults",
        "secrets",
        "mcp",
        "remote",
        "serve",
        "microsandbox",
        "notify",
    ] {
        assert!(properties.contains_key(table), "{table}");
    }
    assert_eq!(schema["additionalProperties"], serde_json::json!(false));
}
