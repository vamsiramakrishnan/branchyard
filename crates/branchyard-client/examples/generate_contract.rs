//! Regenerates `schema/contract.json` from the Rust wire types.
//!
//! ```sh
//! cargo run -p branchyard-client --features schema --example generate_contract > schema/contract.json
//! ```
//!
//! `tests/contract.rs` (also behind `schema`) checks that the checked-in
//! file is what this prints, so a changed wire type without a regenerated
//! schema fails CI.
#[cfg(feature = "schema")]
fn main() {
    print!("{}", branchyard_client::schema::contract_json());
}

#[cfg(not(feature = "schema"))]
fn main() {
    eprintln!("generate_contract needs --features schema: cargo run -p branchyard-client --features schema --example generate_contract");
    std::process::exit(1);
}
