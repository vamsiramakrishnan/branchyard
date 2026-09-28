//! `schema/server.config.json` freshness: regenerate it in-process and
//! compare with the checked-in file, byte for byte. A configuration type
//! (`crates/branchyard-server/src/config.rs`'s `FileConfig` and its
//! nested types) changed without
//! `cargo run -p branchyard-server --features schema --example
//! generate_server_config_schema > schema/server.config.json` fails
//! here, in CI.
//!
//! Only runs with `--features schema`
//! (`cargo test -p branchyard-server --locked --offline --features schema`);
//! without it, this file compiles to zero tests rather than failing, so
//! `cargo test --workspace` (no features) still passes.
#![cfg(feature = "schema")]

use std::path::Path;

#[test]
fn server_config_json_matches_the_generated_schema() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/branchyard-server has two parent directories");
    let checked_in = std::fs::read_to_string(root.join("schema/server.config.json"))
        .expect("schema/server.config.json is checked in");
    let generated = branchyard_server::schema::server_config_json();
    assert_eq!(
        checked_in, generated,
        "schema/server.config.json is stale; regenerate with:\n\
         cargo run -p branchyard-server --features schema --example \
         generate_server_config_schema > schema/server.config.json"
    );
}
