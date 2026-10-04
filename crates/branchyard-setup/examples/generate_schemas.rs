//! Regenerates `schema/branchyard.config.json` and
//! `schema/setup.protocol.json`:
//!
//! ```sh
//! cargo run -p branchyard-setup --example generate_schemas
//! ```
//!
//! `tests/schema.rs` fails when either checked-in file differs from what
//! this writes.
#![allow(clippy::expect_used)] // tests: a panic is the failure report
use std::path::Path;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema");
    for (name, text) in [
        (
            "branchyard.config.json",
            branchyard_setup::schema::config_json(),
        ),
        (
            "setup.protocol.json",
            branchyard_setup::schema::protocol_json(),
        ),
    ] {
        std::fs::write(root.join(name), text).expect("schema/ is writable");
        println!("wrote schema/{name}");
    }
}
