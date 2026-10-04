//! Running the `git` executable with argument vectors: the one place in
//! Branchyard that starts `git` on the host. Every invocation goes through
//! [`Git`], which runs in a given directory with no stdin, no terminal
//! prompt, the C locale, and without the `GIT_DIR`-style variables that
//! would point it at another repository; its failures are [`GitError`]s
//! carrying the arguments, exit code and stderr. The engine
//! (`branchyard::git`, checkpoint refs, and `by try`'s patches and blobs,
//! fed through [`Git::stdin`]), the CLI (`by pr`'s branch names and
//! diffstat), the server and the Substrate transfer all use it rather than
//! a `Command` of their own.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::BranchName;

/// Variables that would redirect git away from the directory we pass. They
/// leak in when Branchyard itself runs under a git hook or `git exec`.
pub(crate) const SCRUBBED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

/// Repository hooks are not run by Branchyard's own commits, merges, and
/// checkouts; acceptance is decided by an explicit [`crate::Check`].
#[cfg(not(windows))]
const NO_HOOKS: &str = "core.hooksPath=/dev/null";
#[cfg(windows)]
const NO_HOOKS: &str = "core.hooksPath=NUL";

/// Errors from git invocations or from repository state Branchyard requires.
#[derive(Debug)]
#[non_exhaustive]
pub enum GitError {
    /// The `git` executable could not be started.
    Spawn(io::Error),
    /// git exited unsuccessfully.
    Failed {
        args: Vec<String>,
        code: Option<i32>,
        stderr: String,
    },
    /// The path is not inside a git working tree.
    NotAWorkTree(PathBuf),
    /// A revision did not resolve to a commit.
    InvalidRevision(String),
    /// A local branch name was rejected.
    InvalidRef(String),
    /// `by/<name>` already exists.
    BranchExists(BranchName),
    /// The workspace's HEAD is not on its `by/<name>` branch.
    NotOnBranch {
        expected: String,
        actual: Option<String>,
    },
    /// The base commit recorded for a workspace branch is missing.
    MissingBase(BranchName),
    /// The workspace head does not descend from its recorded base.
    NotDescendant { base: String, head: String },
    /// Filesystem error outside git.
    Io(io::Error),
    /// git produced output this crate could not interpret.
    Parse(String),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(e) => write!(f, "could not run git: {e}"),
            Self::Failed { args, code, stderr } => {
                write!(f, "git {} failed", args.join(" "))?;
                if let Some(code) = code {
                    write!(f, " with exit code {code}")?;
                }
                if !stderr.trim().is_empty() {
                    write!(f, ": {}", stderr.trim())?;
                }
                Ok(())
            }
            Self::NotAWorkTree(p) => write!(f, "{} is not a git working tree", p.display()),
            Self::InvalidRevision(r) => write!(f, "{r:?} does not name a commit"),
            Self::InvalidRef(r) => write!(f, "{r:?} is not a valid local branch name"),
            Self::BranchExists(b) => write!(f, "branch {} already exists", b.branch()),
            Self::NotOnBranch { expected, actual } => write!(
                f,
                "workspace HEAD is on {} instead of {expected}",
                actual.as_deref().unwrap_or("a detached commit")
            ),
            Self::MissingBase(b) => write!(f, "no base commit recorded for {}", b.branch()),
            Self::NotDescendant { base, head } => {
                write!(f, "{head} does not descend from base {base}")
            }
            Self::Io(e) => write!(f, "{e}"),
            Self::Parse(m) => write!(f, "unexpected git output: {m}"),
        }
    }
}

impl std::error::Error for GitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(e) | Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for GitError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// A git invocation rooted at one directory.
pub struct Git {
    cmd: Command,
    args: Vec<String>,
    input: Option<Vec<u8>>,
}

impl Git {
    pub fn new(dir: &Path) -> Self {
        let mut cmd = Command::new("git");
        cmd.current_dir(dir)
            .stdin(Stdio::null())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C");
        for var in SCRUBBED_ENV {
            cmd.env_remove(var);
        }
        Self {
            cmd,
            args: Vec::new(),
            input: None,
        }
    }

    /// Feed `bytes` to git's stdin (instead of none), as `git apply` and
    /// `git hash-object --stdin` read it.
    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.input = Some(bytes.into());
        self
    }

    /// For commands that commit, merge, or check out on Branchyard's behalf.
    pub fn no_hooks(self) -> Self {
        self.arg("-c").arg(NO_HOOKS)
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        let arg = arg.as_ref();
        self.args.push(arg.to_string_lossy().into_owned());
        self.cmd.arg(arg);
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for a in args {
            self = self.arg(a);
        }
        self
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.cmd.env(key, value);
        self
    }

    /// Runs git and returns its output whatever the exit status.
    pub fn output(mut self) -> Result<(Output, Vec<String>), GitError> {
        let Some(input) = self.input.take() else {
            let out = self.cmd.output().map_err(GitError::Spawn)?;
            return Ok((out, self.args));
        };
        let mut child = self
            .cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(GitError::Spawn)?;
        let stdin = child.stdin.take();
        // Written from another thread so a large input cannot deadlock
        // against git filling its stdout pipe.
        let writer = std::thread::spawn(move || {
            use std::io::Write;
            if let Some(mut stdin) = stdin {
                // git may exit before reading everything (a failed apply);
                // its exit status says so, not the broken pipe.
                let _ = stdin.write_all(&input);
            }
        });
        let out = child.wait_with_output().map_err(GitError::Io)?;
        branchyard_support::join_reporting("writer", writer);
        Ok((out, self.args))
    }

    /// Runs git, requiring success; returns stdout's bytes unchanged.
    pub fn run_bytes(self) -> Result<Vec<u8>, GitError> {
        let (out, args) = self.output()?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(failed(args, &out))
        }
    }

    /// Runs git, requiring success; returns stdout.
    pub fn run(self) -> Result<String, GitError> {
        let (out, args) = self.output()?;
        if out.status.success() {
            String::from_utf8(out.stdout).map_err(|e| GitError::Parse(e.to_string()))
        } else {
            Err(failed(args, &out))
        }
    }

    /// Runs git and says whether it exited successfully, for commands
    /// whose failure is an answer (any non-zero exit is `false`).
    pub fn succeeds(self) -> Result<bool, GitError> {
        let (out, _) = self.output()?;
        Ok(out.status.success())
    }

    /// Runs git for a yes/no answer: exit 0 is true, exit 1 is false.
    pub fn test(self) -> Result<bool, GitError> {
        let (out, args) = self.output()?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(failed(args, &out)),
        }
    }
}

/// The error for a git run that exited unsuccessfully.
pub fn failed(args: Vec<String>, out: &Output) -> GitError {
    GitError::Failed {
        args,
        code: out.status.code(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// `-c user.name=...`/`-c user.email=...` when the repository has no
/// identity configured, so Branchyard's commits do not fail on fresh hosts.
pub(crate) fn identity_args(dir: &Path) -> Result<Vec<&'static str>, GitError> {
    let mut args = Vec::new();
    for (key, value) in [
        ("user.name", "user.name=Branchyard"),
        ("user.email", "user.email=branchyard@localhost"),
    ] {
        if !Git::new(dir).args(["config", "--get", key]).test()? {
            args.extend(["-c", value]);
        }
    }
    Ok(args)
}

/// Full object IDs only (SHA-1 or SHA-256), lowercase hex.
pub(crate) fn is_object_id(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// One entry of `git worktree list --porcelain -z`.
#[derive(Debug)]
pub(crate) struct WorktreeEntry {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub bare: bool,
    pub prunable: bool,
}

pub(crate) fn worktrees(dir: &Path) -> Result<Vec<WorktreeEntry>, GitError> {
    let out = Git::new(dir)
        .args(["worktree", "list", "--porcelain", "-z"])
        .run()?;
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for field in out.split('\0') {
        if field.is_empty() {
            entries.extend(current.take());
            continue;
        }
        let (key, value) = field.split_once(' ').unwrap_or((field, ""));
        if key == "worktree" {
            entries.extend(current.take());
            current = Some(WorktreeEntry {
                path: PathBuf::from(value),
                branch: None,
                bare: false,
                prunable: false,
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            return Err(GitError::Parse(format!(
                "worktree field before path: {field}"
            )));
        };
        match key {
            "branch" => entry.branch = Some(value.to_owned()),
            "bare" => entry.bare = true,
            "prunable" => entry.prunable = true,
            _ => {}
        }
    }
    entries.extend(current);
    Ok(entries)
}

/// The branch checked out in the working tree at `dir` (such as `main`),
/// or `None` when HEAD is detached.
pub fn current_branch(dir: &Path) -> Result<Option<String>, GitError> {
    crate::repo::current_branch_in(dir)
}
