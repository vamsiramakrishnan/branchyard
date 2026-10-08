//! The environment a test's `by`, server or SDK process starts with.
use std::ffi::OsStr;
use std::process::Command;

/// `command` without the environment of a Branchyard turn the tests may be
/// running in. Every test that starts `by`, a Branchyard server or bridge,
/// or the Python SDK starts it through this (or [`crate::Repo`], which
/// does).
///
/// - Every `BRANCHYARD_*` variable is removed: inherited, they make the
///   process act as the delegated branch running the tests
///   (`BRANCHYARD_DELEGATION`, `BRANCHYARD_BRANCH`), in its yard
///   (`BRANCHYARD_ROOT`, `BRANCHYARD_WORKTREE`), or against a remote
///   (`BRANCHYARD_REMOTE`, ...). Set the ones a test means after this.
/// - `GIT_CEILING_DIRECTORIES` is the temporary directory. A turn's
///   `TMPDIR` is under its yard's `.branchyard/`, so a [`crate::Scratch`]
///   directory that holds no repository would otherwise be inside the
///   yard's working tree, and `by` run there would open that yard. A
///   repository inside a scratch directory is found as usual. `TMPDIR`
///   itself is kept, so the process's temporary files stay where the
///   test's do.
pub fn hermetic(command: &mut Command) -> &mut Command {
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(|n| n.starts_with("BRANCHYARD_")) {
            command.env_remove(&name);
        }
    }
    let tmp = std::env::temp_dir();
    let tmp = std::fs::canonicalize(&tmp).unwrap_or(tmp);
    command.env("GIT_CEILING_DIRECTORIES", OsStr::new(&tmp))
}
