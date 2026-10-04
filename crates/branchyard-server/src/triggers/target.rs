// Derived from stablyai/orca src/main/automations/run-target-resolution.ts,
// at revision 280733273545f0b3eeedc1be54b14d406239030e.
// Copyright (c) 2026 Lovecast Inc. Licensed under the MIT License; the
// license text, which must accompany substantial portions of this code, is
// in vendor/orca/LICENSE.
// Modified for Branchyard: translated from TypeScript to Rust. Orca
// resolves an automation's host, project setup and repository; a
// Branchyard trigger names a served repository, so what is kept is the
// shape: the target is checked at fire time, not trusted from creation,
// and each diagnosis is one fixed sentence (Orca's reason: runs that fail
// the same way read the same, and a varying message would hide that).
// The checks are Branchyard's: the repository is still served by this
// process under the trigger's name, and its root is still a git checkout.

//! Where a trigger's run executes, resolved when it fires.

use std::path::{Path, PathBuf};

/// The repository is not served by this server or worker.
pub const NOT_SERVED: &str =
    "The trigger's repository is not served here, so this trigger has nowhere to run.";
/// The served path is gone.
pub const PATH_GONE: &str = "The trigger's repository path no longer exists on this server.";
/// The served path is no longer a git checkout.
pub const NOT_A_CHECKOUT: &str = "The trigger's repository path is no longer a git checkout.";

/// The root of `repo` among `served` (name, root), or the one sentence
/// that says why the run has nowhere to go.
pub fn resolve(repo: &str, served: &[(String, PathBuf)]) -> Result<PathBuf, &'static str> {
    let Some((_, root)) = served.iter().find(|(name, _)| name == repo) else {
        return Err(NOT_SERVED);
    };
    if !root.is_dir() {
        return Err(PATH_GONE);
    }
    if !is_checkout(root) {
        return Err(NOT_A_CHECKOUT);
    }
    Ok(root.clone())
}

fn is_checkout(root: &Path) -> bool {
    root.join(".git").exists()
}

#[allow(clippy::let_underscore_must_use)] // tests: a panic is the failure report
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_diagnosis_is_one_fixed_sentence() {
        let dir = std::env::temp_dir().join(format!("by-target-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let served = vec![
            ("app".to_owned(), dir.clone()),
            ("gone".to_owned(), dir.join("missing")),
            ("plain".to_owned(), std::env::temp_dir()),
        ];
        assert_eq!(resolve("app", &served), Ok(dir.clone()));
        assert_eq!(resolve("docs", &served), Err(NOT_SERVED));
        assert_eq!(resolve("gone", &served), Err(PATH_GONE));
        if !std::env::temp_dir().join(".git").exists() {
            assert_eq!(resolve("plain", &served), Err(NOT_A_CHECKOUT));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
