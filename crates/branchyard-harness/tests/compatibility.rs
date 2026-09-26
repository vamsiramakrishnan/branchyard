//! `docs/compatibility.md` is generated; this fails when it is stale.

#[path = "../examples/compat_matrix.rs"]
#[allow(dead_code)]
mod compat_matrix;

#[test]
fn the_compatibility_matrix_is_current() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/compatibility.md");
    let committed = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        committed == compat_matrix::render(),
        "docs/compatibility.md is stale; regenerate it with\n  \
         cargo run -p branchyard-harness --example compat_matrix > docs/compatibility.md"
    );
}
