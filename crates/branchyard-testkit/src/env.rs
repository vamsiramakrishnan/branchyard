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

/// Populate a Unix test `PATH` with system utilities and no real agent harnesses.
///
/// Put a directory containing fake harnesses before this directory. Keeping
/// `/usr/bin` off the resulting `PATH` prevents an installed harness from
/// unexpectedly being detected or launched by a hermetic test.
///
/// # Errors
///
/// Returns filesystem errors while creating the directory or utility links.
#[cfg(unix)]
pub fn system_tools(directory: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)?;
    for tool in [
        "sh", "git", "python3", "mktemp", "mkdir", "rm", "hostname", "uname", "sleep", "tr",
        "grep", "head", "cut", "sed", "cat", "dirname", "readlink", "env", "id", "stat", "chmod",
        "cp", "mv", "ln", "touch", "find", "sort",
    ] {
        let source = ["/usr/bin", "/bin"]
            .into_iter()
            .map(|dir| std::path::Path::new(dir).join(tool))
            .find(|path| path.is_file());
        if let Some(source) = source {
            std::os::unix::fs::symlink(source, directory.join(tool))?;
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn utility_path_contains_shell_tools_without_real_harnesses() {
        let scratch = crate::Scratch::new("system-tools");
        let tools = scratch.path().join("bin");
        super::system_tools(&tools).unwrap();
        assert!(tools.join("sh").is_file());
        assert!(tools.join("git").is_file());
        for harness in [
            "agy",
            "antigravity",
            "gemini",
            "claude",
            "codex",
            "opencode",
        ] {
            assert!(
                !tools.join(harness).exists(),
                "{harness} must be a fake supplied by the test"
            );
        }
    }
}
