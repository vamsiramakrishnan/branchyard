//! `deploy/config.example.json` parses against today's configuration
//! schema (`crates/branchyard-server/src/config.rs`), so the deploy recipe
//! and the schema cannot silently drift apart. See docs/deploy.md.
//!
//! Docker secret paths (`token_file`) do not exist in this test, so it
//! patches the checked-in example onto a temporary token file with the
//! same one-token-per-line layout before loading it; that is the only
//! change made to its content.
use std::fs;
use std::path::Path;

#[test]
fn deploy_config_example_matches_the_configuration_schema() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/branchyard-server has two parent directories");
    let text = fs::read_to_string(root.join("deploy/config.example.json"))
        .expect("deploy/config.example.json is checked in");

    let temp = tempfile::Builder::new()
        .prefix("branchyard-deploy-config-")
        .tempdir()
        .unwrap();
    let dir = temp.path();
    fs::write(dir.join("token.txt"), "0123456789abcdef\n").unwrap();
    let patched = text.replace(
        "/run/secrets/branchyard_token",
        &dir.join("token.txt").to_string_lossy(),
    );
    let path = dir.join("server.json");
    fs::write(&path, patched).unwrap();

    let partial = branchyard_server::config::load_file(&path)
        .unwrap_or_else(|error| panic!("deploy/config.example.json does not parse: {error}"));
    assert_eq!(partial.tokens.len(), 1);
    assert_eq!(partial.tokens[0].name, "ci");
    assert_eq!(
        partial.repos,
        [("app".to_owned(), Path::new("/repos/app").to_owned())]
    );
    assert_eq!(partial.data_dir, Some(Path::new("/data").to_owned()));
}
