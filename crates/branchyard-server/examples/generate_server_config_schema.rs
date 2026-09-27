//! Regenerates `schema/server.config.json` from the configuration file's
//! Rust types (`crates/branchyard-server/src/config.rs`).
//!
//! ```sh
//! cargo run -p branchyard-server --features schema --example generate_server_config_schema \
//!     > schema/server.config.json
//! ```
//!
//! `tests/server_config_schema.rs` (also behind `schema`) checks that the
//! checked-in file is what this prints, so a changed configuration type
//! without a regenerated schema fails CI.
#[cfg(feature = "schema")]
fn main() {
    print!("{}", branchyard_server::schema::server_config_json());
}

#[cfg(not(feature = "schema"))]
fn main() {
    eprintln!(
        "generate_server_config_schema needs --features schema: cargo run -p branchyard-server \
         --features schema --example generate_server_config_schema"
    );
    std::process::exit(1);
}
