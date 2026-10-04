#![allow(clippy::expect_used, clippy::panic)] // tests: a panic is the failure report
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use branchyard_support::LockExt;

/// The `fake-acp-agent` binary from branchyard-runtime, built once per
/// target directory and profile.
///
/// Cargo exposes a binary's path (`CARGO_BIN_EXE_*`) only to the tests of
/// its own package, and a macro such as `env!` expands in the consumer, so
/// the caller passes one path inside the build it is running in: the `by`
/// binary (`Path::new(env!("CARGO_BIN_EXE_by"))`), or any file under
/// `target/<profile>/`, such as the test executable
/// ([`fake_agent_here`]).
pub fn fake_agent(artifact: &Path) -> &'static Path {
    built("branchyard-runtime", "fake-acp-agent", artifact)
}

/// `fake_agent!()`: [`fake_agent`] for the `by` binary of the crate this
/// expands in (only the `branchyard-cli` tests have one). `env!` expands in
/// the caller, which is why this is a macro and not a function.
#[macro_export]
macro_rules! fake_agent {
    () => {
        $crate::fake_agent(::std::path::Path::new(env!("CARGO_BIN_EXE_by")))
    };
}

/// [`fake_agent`] for the running test executable.
pub fn fake_agent_here() -> &'static Path {
    fake_agent(&std::env::current_exe().expect("the path of the test executable"))
}

/// [`fake_agent_here`], built without coverage instrumentation; see
/// [`built_uninstrumented`] for when a test needs it.
pub fn fake_agent_here_uninstrumented() -> &'static Path {
    built_uninstrumented(
        "branchyard-runtime",
        "fake-acp-agent",
        &std::env::current_exe().expect("the path of the test executable"),
    )
}

/// Build `bin` of `package` into the target directory and profile of
/// `artifact` (a path inside `target/<profile>/`, or its `deps/`), once,
/// and return its path. A failed build fails the test with cargo's status.
/// Under `cargo llvm-cov` the binary is instrumented like the tests, so
/// what it runs counts toward its crate's coverage.
pub fn built(package: &str, bin: &str, artifact: &Path) -> &'static Path {
    build(package, bin, artifact, false)
}

/// [`built`], but never instrumented for coverage: for a helper the engine
/// runs in a sandbox with a cleaned environment, where an instrumented
/// binary (no `LLVM_PROFILE_FILE`) writes a `default_*.profraw` into its
/// working directory, which the engine then reports as a change the branch
/// made. Outside a coverage run it is the same binary as [`built`]'s.
pub fn built_uninstrumented(package: &str, bin: &str, artifact: &Path) -> &'static Path {
    build(package, bin, artifact, true)
}

fn build(package: &str, bin: &str, artifact: &Path, plain: bool) -> &'static Path {
    static BUILT: Mutex<BTreeMap<(PathBuf, String), &'static Path>> = Mutex::new(BTreeMap::new());
    let profile_dir = profile_dir(artifact);
    // Held across the build: concurrent tests wait for the one build.
    let mut built = BUILT.lock_recovering("testkit built binaries");
    let key = (profile_dir.clone(), format!("{package}/{bin}/{plain}"));
    if let Some(path) = built.get(&key) {
        return path;
    }
    let target_dir = profile_dir
        .parent()
        .unwrap_or_else(|| panic!("{} has no target directory", profile_dir.display()));
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Command::new(cargo);
    command
        .args(["build", "--quiet", "--offline", "--manifest-path"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.toml"))
        .args(["-p", package, "--bin", bin]);
    // A plain build under `cargo llvm-cov` goes to a target directory of its
    // own: cargo's fingerprint does not see the RUSTC_WRAPPER that
    // instruments, so it would reuse the instrumented binary in the shared one.
    let coverage = std::env::var_os("CARGO_LLVM_COV").is_some()
        || std::env::var_os("LLVM_PROFILE_FILE").is_some();
    let (target_dir, built_dir) = if plain && coverage {
        command.env_remove("RUSTC_WRAPPER");
        for flags in ["CARGO_ENCODED_RUSTFLAGS", "RUSTFLAGS"] {
            command.env_remove(flags);
        }
        let own = target_dir.join("uninstrumented");
        let profile = profile_dir.file_name().unwrap_or("debug".as_ref());
        let built_dir = own.join(profile);
        (own, built_dir)
    } else {
        (target_dir.to_path_buf(), profile_dir.clone())
    };
    command.env("CARGO_TARGET_DIR", &target_dir);
    match profile_dir.file_name().and_then(|n| n.to_str()) {
        Some("debug") => {}
        Some("release") => {
            command.arg("--release");
        }
        Some(other) => {
            command.args(["--profile", other]);
        }
        None => panic!("unexpected build location {}", artifact.display()),
    }
    let status = command
        .status()
        .unwrap_or_else(|e| panic!("run cargo to build {bin}: {e}"));
    assert!(
        status.success(),
        "building {bin} of {package} failed: {status}"
    );
    let path = built_dir.join(bin);
    assert!(path.is_file(), "{} was not built", path.display());
    let path: &'static Path = Box::leak(path.into_boxed_path());
    built.insert(key, path);
    path
}

/// `target/<profile>` for a path inside it (directly, or in `deps/`).
fn profile_dir(artifact: &Path) -> PathBuf {
    let dir = artifact
        .parent()
        .unwrap_or_else(|| panic!("{} has no parent directory", artifact.display()));
    if dir.file_name().is_some_and(|n| n == "deps") {
        dir.parent().expect("deps has a parent").to_path_buf()
    } else {
        dir.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_directory_is_found_from_a_binary_or_a_test_executable() {
        assert_eq!(profile_dir(Path::new("/t/debug/by")), Path::new("/t/debug"));
        assert_eq!(
            profile_dir(Path::new("/t/debug/deps/cli-1a2b")),
            Path::new("/t/debug")
        );
    }
}
