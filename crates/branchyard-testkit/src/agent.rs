use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

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

/// Build `bin` of `package` into the target directory and profile of
/// `artifact` (a path inside `target/<profile>/`, or its `deps/`), once,
/// and return its path. A failed build fails the test with cargo's status.
pub fn built(package: &str, bin: &str, artifact: &Path) -> &'static Path {
    static BUILT: Mutex<BTreeMap<(PathBuf, String), &'static Path>> = Mutex::new(BTreeMap::new());
    let profile_dir = profile_dir(artifact);
    // Held across the build: concurrent tests wait for the one build.
    let mut built = BUILT.lock().unwrap_or_else(|e| e.into_inner());
    let key = (profile_dir.clone(), format!("{package}/{bin}"));
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
        .args(["-p", package, "--bin", bin])
        .env("CARGO_TARGET_DIR", target_dir);
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
    let path = profile_dir.join(bin);
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
