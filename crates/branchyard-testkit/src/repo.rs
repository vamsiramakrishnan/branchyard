#![allow(clippy::expect_used, clippy::panic)] // tests: a panic is the failure report
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

use crate::{fake_agent, hermetic, Scratch};

/// A throwaway git repository on branch `main`, with one commit, driven
/// through the built `by` binary. Hermetic: no global or system git
/// configuration, no user Branchyard configuration, no color or pager.
///
/// `dir` is a scratch directory removed on drop; the repository is its
/// `repo/` subdirectory, `root`. A command that fails shows its arguments,
/// stdout and stderr.
///
/// Wrap it for a test file's own extras:
///
/// ```text
/// struct Repo(branchyard_testkit::Repo);
/// impl std::ops::Deref for Repo { type Target = branchyard_testkit::Repo; fn deref(&self) -> &Self::Target { &self.0 } }
/// ```
pub struct Repo {
    /// The scratch directory: the repository's parent, for files that
    /// belong beside it, not in it.
    pub dir: PathBuf,
    /// The repository's working tree (`<dir>/repo`).
    pub root: PathBuf,
    by: PathBuf,
    envs: Vec<(String, Option<String>)>,
    _scratch: Scratch,
}

impl Repo {
    /// A repository whose first commit holds `a.txt` ("one\n").
    /// `by` is the built binary: `Path::new(env!("CARGO_BIN_EXE_by"))`.
    pub fn init(by: impl Into<PathBuf>) -> Repo {
        Repo::init_with(by, &[("a.txt", "one\n")])
    }

    /// A repository whose first commit holds `files` (path, contents).
    pub fn init_with(by: impl Into<PathBuf>, files: &[(&str, &str)]) -> Repo {
        let scratch = Scratch::new("test");
        let dir = scratch.path().to_path_buf();
        let root = dir.join("repo");
        std::fs::create_dir_all(&root).expect("create the repository directory");
        let repo = Repo {
            dir,
            root,
            by: by.into(),
            envs: Vec::new(),
            _scratch: scratch,
        };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.name", "Test"]);
        repo.git(&["config", "user.email", "test@localhost"]);
        for (name, text) in files {
            if let Some(parent) = repo.root.join(name).parent() {
                std::fs::create_dir_all(parent).expect("create a parent directory");
            }
            std::fs::write(repo.root.join(name), text)
                .unwrap_or_else(|e| panic!("write {name}: {e}"));
        }
        repo.git(&["add", "."]);
        repo.git(&["commit", "-q", "-m", "initial"]);
        repo
    }

    /// Set `key` for every command this repository runs.
    pub fn set_env(&mut self, key: &str, value: &str) {
        self.envs.push((key.to_owned(), Some(value.to_owned())));
    }

    /// Remove `key` from every command this repository runs.
    pub fn remove_env(&mut self, key: &str) {
        self.envs.push((key.to_owned(), None));
    }

    /// The path of the built `by` binary.
    pub fn by_path(&self) -> &Path {
        &self.by
    }

    /// `program` in the repository, with the hermetic environment.
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        let mut command = Command::new(program);
        hermetic(&mut command)
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("NO_COLOR", "1")
            // Never a person's own ~/.config/branchyard/config.toml.
            .env("BRANCHYARD_USER_CONFIG", self.dir.join("user/config.toml"))
            .env("PAGER", "cat");
        for (key, value) in &self.envs {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        command
    }

    /// `git <args>`; fails the test, showing its output, unless it succeeds.
    pub fn git(&self, args: &[&str]) -> String {
        let out = self.command("git").args(args).output().expect("run git");
        assert!(out.status.success(), "git {args:?}\n{}", shown(&out));
        String::from_utf8(out.stdout).expect("git printed UTF-8")
    }

    /// `by <args>`, whatever its status.
    pub fn by(&self, args: &[&str]) -> Output {
        self.command(&self.by)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("run {}: {e}", self.by.display()))
    }

    /// `by <args>`; fails the test, showing its output, unless it succeeds.
    pub fn ok(&self, args: &[&str]) -> Output {
        let out = self.by(args);
        assert!(out.status.success(), "by {args:?}\n{}", shown(&out));
        out
    }

    /// `by <args>` with `input` on its standard input; fails the test,
    /// showing its output, unless it succeeds.
    pub fn by_with_stdin(&self, args: &[&str], input: &str) -> Output {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = self
            .command(&self.by)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("run {}: {e}", self.by.display()));
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(input.as_bytes())
            .expect("write to by's standard input");
        child.wait_with_output().expect("wait for by")
    }

    /// `by <args>` with the fake agent as the gemini-cli harness (unless
    /// `send` or an explicit `--harness`).
    pub fn by_agent(&self, args: &[&str]) -> Output {
        let agent = fake_agent(&self.by).display().to_string();
        let mut all: Vec<&str> = args.to_vec();
        all.extend(["--command", &agent]);
        if args[0] != "send" && !args.contains(&"--harness") {
            all.extend(["--harness", "gemini-cli"]);
        }
        self.by(&all)
    }

    /// `by <args>` that must succeed and print JSON.
    pub fn json(&self, args: &[&str]) -> Value {
        let out = self.ok(args);
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("by {args:?} did not print JSON ({e})\n{}", shown(&out)))
    }
}

/// The status, stdout and stderr of `out`, for a failure message.
fn shown(out: &Output) -> String {
    format!(
        "status: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}
