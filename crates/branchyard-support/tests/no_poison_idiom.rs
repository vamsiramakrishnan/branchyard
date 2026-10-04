//! The guard: no crate retypes the poison-recovery idiom. Use
//! `branchyard_support::LockExt` (and friends), which log the poisoning with
//! the lock's name. This runs with the crate's tests in CI.

use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The idiom in any spelling: whitespace and the closure's parameter name
/// vary, so compare with whitespace removed.
pub fn uses_the_idiom(source: &str) -> bool {
    let squeezed: String = source.chars().filter(|c| !c.is_whitespace()).collect();
    if squeezed.contains("unwrap_or_else(PoisonError::into_inner)")
        || squeezed.contains("unwrap_or_else(std::sync::PoisonError::into_inner)")
    {
        return true;
    }
    let mut rest = squeezed.as_str();
    while let Some(at) = rest.find("unwrap_or_else(|") {
        rest = &rest[at + "unwrap_or_else(|".len()..];
        let Some(bar) = rest.find('|') else { break };
        let param = &rest[..bar];
        let body = &rest[bar + 1..];
        if !param.is_empty()
            && param.chars().all(|c| c.is_alphanumeric() || c == '_')
            && body.starts_with(&format!("{param}.into_inner())"))
        {
            return true;
        }
    }
    false
}

#[test]
fn no_crate_retypes_the_poison_recovery_idiom() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    for entry in fs::read_dir(&crates).unwrap().flatten() {
        rust_files(&entry.path().join("src"), &mut files);
        rust_files(&entry.path().join("tests"), &mut files);
    }
    assert!(
        files.len() > 100,
        "the scan found only {} files",
        files.len()
    );
    // This crate's own sources spell the idiom out in its docs and tests.
    let exempt = |f: &Path| {
        f.ends_with("branchyard-support/src/locks.rs")
            || f.ends_with("branchyard-support/tests/no_poison_idiom.rs")
    };
    let mut offenders: Vec<String> = files
        .iter()
        .filter(|f| !exempt(f))
        .filter(|f| uses_the_idiom(&fs::read_to_string(f).unwrap()))
        .map(|f| f.strip_prefix(&crates).unwrap().display().to_string())
        .collect();
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "use branchyard_support::LockExt::lock_recovering(\"name\") instead of \
         `unwrap_or_else(|e| e.into_inner())` in:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_detector_sees_every_spelling() {
    for spelling in [
        "m.lock().unwrap_or_else(|e| e.into_inner())",
        "m.lock().unwrap_or_else(|p| p.into_inner())",
        "m.lock()\n    .unwrap_or_else(|poisoned| poisoned.into_inner())",
        "m.lock().unwrap_or_else(PoisonError::into_inner)",
    ] {
        assert!(uses_the_idiom(spelling), "missed {spelling:?}");
    }
    for fine in [
        "m.lock_recovering(\"m\")",
        "x.unwrap_or_else(|e| e.to_string())",
        "x.unwrap_or_else(|e| other.into_inner())",
    ] {
        assert!(!uses_the_idiom(fine), "flagged {fine:?}");
    }
}
