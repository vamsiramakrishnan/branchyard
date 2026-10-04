//! The provider seam: what a `Provider` variant means is answered by
//! `ProviderKind` in `crates/branchyard/src/providers/`, never by a match
//! on the enum somewhere else.
//!
//! This reads every crate's `src/` and counts the places the four variants
//! are named (`Provider::Local`, `::Microsandbox`, `::Substrate`,
//! `::Recipe`) outside `crates/branchyard/src/providers/`. In the engine
//! crate the allowed count is zero. A few surfaces still match on the enum
//! to announce or admit a provider; they are a ratchet: each file's count
//! is listed below and may only fall. Adding a variant's name anywhere not
//! listed fails; removing one fails too until the list is lowered, so the
//! number can never creep back up.
//!
//! See "Adding a provider" in `docs/providers.md`.

use std::fs;
use std::path::{Path, PathBuf};

/// The variants, as the enum spells them.
const VARIANTS: [&str; 4] = [
    "Provider::Local",
    "Provider::Microsandbox",
    "Provider::Substrate",
    "Provider::Recipe",
];

/// The one directory where a variant may be named freely.
const SEAM: &str = "crates/branchyard/src/providers/";

/// Files outside the seam that still name a variant, and how many times.
/// Lower these as the sites move onto `ProviderKind`; never raise one.
///
/// - `branchyard-cli`: announcing the chosen provider (`commands.rs`,
///   `remote.rs`), building it from flags (`commands.rs`), and a local-only
///   check (`harness_cmd.rs`).
/// - `branchyard-server`: admitting a requested provider against
///   `allow_providers` (`api.rs`). These crates cannot reach the
///   crate-private trait; they need a public accessor first.
const RATCHET: &[(&str, usize)] = &[
    ("crates/branchyard-cli/src/commands.rs", 8),
    ("crates/branchyard-cli/src/harness_cmd.rs", 1),
    ("crates/branchyard-cli/src/remote.rs", 5),
    ("crates/branchyard-server/src/api.rs", 6),
];

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root")
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// How many times `text` names a variant, outside comments. A longer name
/// that ends the same way (`SandboxProvider::Local`) is not one.
fn named(text: &str) -> usize {
    let mut count = 0;
    for line in text.lines() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for variant in VARIANTS {
            let mut rest = line;
            while let Some(at) = rest.find(variant) {
                let before = rest[..at].chars().next_back();
                let after = rest[at + variant.len()..].chars().next();
                let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
                if !word(before) && !word(after) {
                    count += 1;
                }
                rest = &rest[at + variant.len()..];
            }
        }
    }
    count
}

#[test]
fn only_the_providers_directory_names_the_variants() {
    let root = workspace();
    let mut files = Vec::new();
    for krate in fs::read_dir(root.join("crates")).unwrap().flatten() {
        let src = krate.path().join("src");
        if src.is_dir() {
            sources(&src, &mut files);
        }
    }
    assert!(files.len() > 100, "found only {} sources", files.len());
    let mut problems = Vec::new();
    let mut ratcheted = vec![false; RATCHET.len()];
    for file in files {
        let rel = file
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel.starts_with(SEAM) {
            continue;
        }
        let found = named(&fs::read_to_string(&file).unwrap());
        let allowed = RATCHET.iter().position(|(path, _)| *path == rel);
        let limit = allowed.map_or(0, |i| RATCHET[i].1);
        if let Some(i) = allowed {
            ratcheted[i] = true;
        }
        if found > limit {
            problems.push(format!(
                "{rel}: names a Provider variant {found} time(s), {limit} allowed. Add the \
                 behaviour to `ProviderKind` and its four implementations in {SEAM} and call \
                 `provider.kind().method()` instead of matching (docs/providers.md, \
                 \"Adding a provider\")"
            ));
        } else if found < limit {
            problems.push(format!(
                "{rel}: names a Provider variant {found} time(s), below its ratchet of {limit}. \
                 Lower the entry in RATCHET in crates/branchyard/tests/provider_seam.rs to \
                 {found} (or remove it at 0) so it cannot rise again"
            ));
        }
    }
    for (i, seen) in ratcheted.iter().enumerate() {
        if !seen {
            problems.push(format!(
                "RATCHET lists {}, which does not exist",
                RATCHET[i].0
            ));
        }
    }
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

#[test]
fn the_counter_sees_what_it_should() {
    assert_eq!(named("match p { Provider::Local => 1, _ => 2 }"), 1);
    assert_eq!(
        named("Some(Provider::Substrate(o)) | Some(branchyard::Provider::Recipe(o))"),
        2
    );
    assert_eq!(named("// Provider::Local is the default"), 0);
    assert_eq!(named("SandboxProvider::Local"), 0);
    assert_eq!(named("Provider::LocalHost"), 0);
}
