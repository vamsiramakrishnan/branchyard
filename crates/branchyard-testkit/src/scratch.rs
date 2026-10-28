#![allow(clippy::panic)] // tests: a panic is the failure report
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A temporary directory, removed when dropped.
///
/// The path is canonical (no symlinked `/tmp`), so it compares equal to what
/// a child process reports as its working directory.
///
/// Whatever still runs in it when it is dropped (a detached workspace
/// script, a server started by hand, the child of a test that failed before
/// its last step) is stopped first, with a line on stderr naming it: a test
/// does not leave processes behind, whether it passed or failed. That needs
/// `/proc`; elsewhere nothing is stopped.
pub struct Scratch {
    dir: Option<tempfile::TempDir>,
    path: PathBuf,
}

impl Scratch {
    /// A fresh directory whose name starts with `branchyard-<prefix>-`.
    pub fn new(prefix: &str) -> Scratch {
        let dir = tempfile::Builder::new()
            .prefix(&format!("branchyard-{prefix}-"))
            .tempdir()
            .unwrap_or_else(|e| panic!("create a scratch directory for {prefix}: {e}"));
        let path = std::fs::canonicalize(dir.path())
            .unwrap_or_else(|e| panic!("canonicalize {}: {e}", dir.path().display()));
        Scratch {
            dir: Some(dir),
            path,
        }
    }

    /// The directory, canonicalized.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A path inside the directory.
    pub fn join(&self, name: impl AsRef<Path>) -> PathBuf {
        self.path.join(name)
    }

    /// Remove the directory now and fail the test if that does not work,
    /// which dropping would not.
    pub fn close(mut self) {
        stop_processes_in(&self.path);
        if let Some(dir) = self.dir.take() {
            dir.close()
                .unwrap_or_else(|e| panic!("remove {}: {e}", self.path.display()));
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        stop_processes_in(&self.path);
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

/// The processes, other than this one, whose working directory is `dir` or
/// inside it, as `/proc` shows them; none without `/proc`.
pub fn processes_in(dir: &Path) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let own = std::process::id();
    entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| *pid != own)
        .filter(|pid| {
            std::fs::read_link(format!("/proc/{pid}/cwd")).is_ok_and(|cwd| cwd.starts_with(dir))
        })
        .collect()
}

/// Stop every process [`processes_in`] finds in `dir` with SIGKILL, and say
/// so on stderr: it was started by a test and outlived it.
fn stop_processes_in(dir: &Path) {
    let left = processes_in(dir);
    if left.is_empty() {
        return;
    }
    for pid in &left {
        let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        eprintln!(
            "testkit: stopping {} (pid {pid}), still running in {} as the test ends",
            name.trim(),
            dir.display()
        );
    }
    // `kill` reports a process that ended meanwhile; that is no failure.
    branchyard_support::best_effort(
        "stop the processes a test left running",
        Command::new("kill")
            .arg("-KILL")
            .args(left.iter().map(u32::to_string))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    );
}
