//! The GitHub CLI, `gh`, run from argument vectors (never a shell) in the
//! repository. It already handles authentication, GitHub Enterprise hosts
//! and the choice of repository from the git remotes, so `by` asks it
//! rather than speaking the GitHub API itself.
//!
//! Every call names `gh` on `PATH`; tests put a fake one first there.

use std::ffi::OsString;
use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde::de::DeserializeOwned;

/// Why a `gh` call did not give an answer.
#[derive(Debug)]
pub enum GhError {
    /// No `gh` on `PATH`.
    Missing,
    /// `gh` is installed but not logged in.
    Unauthenticated(String),
    /// `gh` ran and failed.
    Failed {
        args: Vec<String>,
        code: Option<i32>,
        stderr: String,
    },
    /// `gh` printed something that is not the JSON asked for.
    Parse {
        args: Vec<String>,
        error: String,
    },
    Io(io::Error),
}

impl fmt::Display for GhError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GhError::Missing => f.write_str(
                "this needs the GitHub CLI, gh, which is not on PATH: install it from \
                 https://cli.github.com and run `gh auth login`",
            ),
            GhError::Unauthenticated(detail) => {
                write!(
                    f,
                    "gh is not logged in to GitHub; run `gh auth login` first"
                )?;
                match detail.trim() {
                    "" => Ok(()),
                    detail => write!(f, " (gh said: {})", one_line(detail)),
                }
            }
            GhError::Failed { args, code, stderr } => {
                write!(f, "gh {} failed", args.join(" "))?;
                if let Some(code) = code {
                    write!(f, " with exit code {code}")?;
                }
                match stderr.trim() {
                    "" => Ok(()),
                    stderr => write!(f, ": {}", one_line(stderr)),
                }
            }
            GhError::Parse { args, error } => {
                write!(
                    f,
                    "gh {} printed unexpected output: {error}",
                    args.join(" ")
                )
            }
            GhError::Io(error) => write!(f, "could not run gh: {error}"),
        }
    }
}

/// At most the first three lines, joined.
fn one_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" / ")
}

/// `gh` in one repository, optionally for one `OWNER/REPO`.
pub struct Gh {
    dir: PathBuf,
    repo: Option<String>,
}

impl Gh {
    /// `gh` run in `dir`, a git working tree whose remotes name the
    /// repository unless `repo` (`OWNER/REPO` or a URL) does.
    pub fn new(dir: &Path, repo: Option<&str>) -> Gh {
        Gh {
            dir: dir.to_path_buf(),
            repo: repo.map(str::to_owned),
        }
    }

    /// Check that `gh` exists and is logged in, so nothing is pushed for a
    /// pull request that cannot be opened.
    pub fn ready(&self) -> Result<(), GhError> {
        let out = self.output(&["auth", "status"], None, false)?;
        if out.status.success() {
            return Ok(());
        }
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        Err(GhError::Unauthenticated(text))
    }

    /// Run `gh args...`, with `-R` for the repository when one was given and
    /// the subcommand takes it, and `stdin` as its input; its stdout on
    /// success.
    pub fn run(&self, args: &[&str], stdin: Option<&str>) -> Result<String, GhError> {
        let out = self.output(args, stdin, true)?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        Err(self.failure(args, &out))
    }

    /// [`Gh::run`], parsing stdout as JSON. Some commands (`gh pr checks`)
    /// exit non-zero while still printing the JSON asked for; that JSON is
    /// taken when `lenient`.
    pub fn json<T: DeserializeOwned>(&self, args: &[&str], lenient: bool) -> Result<T, GhError> {
        let out = self.output(args, None, true)?;
        let usable = out.status.success() || (lenient && !out.stdout.is_empty());
        if !usable {
            return Err(self.failure(args, &out));
        }
        serde_json::from_slice(&out.stdout).map_err(|error| GhError::Parse {
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            error: error.to_string(),
        })
    }

    fn failure(&self, args: &[&str], out: &Output) -> GhError {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if stderr.contains("gh auth login") {
            return GhError::Unauthenticated(stderr);
        }
        GhError::Failed {
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            code: out.status.code(),
            stderr,
        }
    }

    fn output(&self, args: &[&str], stdin: Option<&str>, repo: bool) -> Result<Output, GhError> {
        let mut argv: Vec<OsString> = args.iter().map(OsString::from).collect();
        // `gh api` takes no -R; its callers name the repository in the path.
        if let (true, Some(name), Some(&first)) = (repo, &self.repo, args.first()) {
            if matches!(first, "pr" | "issue" | "run") {
                argv.push("-R".into());
                argv.push(name.into());
            }
        }
        let mut command = Command::new("gh");
        command
            .args(&argv)
            .current_dir(&self.dir)
            .stdin(match stdin {
                Some(_) => Stdio::piped(),
                None => Stdio::null(),
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Never a prompt, a pager or color codes in parsed output.
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_PAGER", "cat")
            .env("NO_COLOR", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1");
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(GhError::Missing),
            Err(error) => return Err(GhError::Io(error)),
        };
        if let (Some(text), Some(mut input)) = (stdin, child.stdin.take()) {
            match input.write_all(text.as_bytes()) {
                Err(error) if error.kind() != io::ErrorKind::BrokenPipe => {
                    return Err(GhError::Io(error))
                }
                _ => {}
            }
        }
        child.wait_with_output().map_err(GhError::Io)
    }
}
