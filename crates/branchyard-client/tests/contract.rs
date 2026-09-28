//! `schema/contract.json` freshness: regenerate it in-process and compare
//! with the checked-in file, byte for byte. A wire type changed without
//! `cargo run -p branchyard-client --features schema --example
//! generate_contract > schema/contract.json` fails here, in CI.
//!
//! Only runs with `--features schema`
//! (`cargo test -p branchyard-client --locked --offline --features schema`);
//! without it, this file compiles to zero tests rather than failing, so
//! `cargo test --workspace` (no features) still passes.
#![cfg(feature = "schema")]

use std::path::Path;

#[test]
fn contract_json_matches_the_generated_schema() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/branchyard-client has two parent directories");
    let checked_in = std::fs::read_to_string(root.join("schema/contract.json"))
        .expect("schema/contract.json is checked in");
    let generated = branchyard_client::schema::contract_json();
    assert_eq!(
        checked_in, generated,
        "schema/contract.json is stale; regenerate with:\n\
         cargo run -p branchyard-client --features schema --example generate_contract \
         > schema/contract.json"
    );
}
