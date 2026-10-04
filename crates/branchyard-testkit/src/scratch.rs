use std::path::{Path, PathBuf};

/// A temporary directory, removed when dropped.
///
/// The path is canonical (no symlinked `/tmp`), so it compares equal to what
/// a child process reports as its working directory.
pub struct Scratch {
    dir: tempfile::TempDir,
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
        Scratch { dir, path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, name: impl AsRef<Path>) -> PathBuf {
        self.path.join(name)
    }

    /// Remove the directory now and fail the test if that does not work,
    /// which dropping would not.
    pub fn close(self) {
        let shown = self.path.clone();
        self.dir
            .close()
            .unwrap_or_else(|e| panic!("remove {}: {e}", shown.display()));
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}
