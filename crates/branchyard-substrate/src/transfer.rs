//! Code in and out of an actor, which has no host mounts: git bundles and
//! directory trees carried by the bridge.
//!
//! [`push`] snapshots the worktree (its `HEAD` commit plus every tracked and
//! untracked, non-ignored file, as a commit on top of `HEAD`), bundles both
//! commits and their history, and recreates the worktree in a fresh
//! repository in the actor: `HEAD` at the same commit, the working files as
//! they were on the host, the index at `HEAD`. [`pull`] snapshots the
//! actor's working files the same way on top of the actor's `HEAD`, brings
//! that commit and the commits below it back in a bundle, and applies the
//! difference between the two snapshots to the host worktree's files:
//! additions, changes, deletions, modes and symbolic links. Commits the
//! harness made in the actor come back as they are (messages, authors,
//! dates, order): the host branch moves to the actor's `HEAD`, and the
//! host's index to that commit, so what the harness left uncommitted is a
//! working-tree change on top of them. The actor's `HEAD` must descend
//! from the commit that was sent; history rewritten below it is refused.
//! When the harness made no commit, the host's index, refs and object
//! store are not touched. Either way the engine's own snapshot then records
//! the candidate as it would for a local harness.
//!
//! Only commits and files cross. The bundles carry objects reachable from
//! the two snapshots; the actor's repository is new, so no host
//! configuration, hook, remote or credential is in it. Ignored files cross
//! in neither direction, and a file the harness wrote outside the actor's
//! worktree is never in the result.
//!
//! On the host, both steps work in a staging repository whose object store
//! falls back to the host repository's (git alternates): snapshot objects
//! and the actor's objects land there, are checked with
//! `transfer.fsckObjects`, and are deleted with it. Only the harness's
//! commits (checked again as they are fetched) reach the host repository's
//! object store, and only the worktree's branch is moved, from exactly the
//! commit that was sent, with hooks disabled.
//!
//! The actor needs `git` on its `PATH`.
//!
//! The steps reach the sandbox through [`Guest`]: a Substrate actor's
//! bridge ([`Endpoint`]), or any [`SandboxProvider`] that can exec
//! ([`Exec`]), whose files cross as `cat` and `tar` streams over the
//! exec's standard input and output (an environment recipe's machine,
//! reached over ssh). The second needs `sh`, `cat`, `tar` and `mkdir`
//! there too, and `tar` on this host.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use branchyard_bridge::Endpoint;
use branchyard_sandbox::{ExecSpec, Process, ProviderError, SandboxProvider};
use branchyard_workspace::Git;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Identity for the snapshot commits, on both sides.
const IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "Branchyard"),
    ("GIT_AUTHOR_EMAIL", "branchyard@localhost"),
    ("GIT_COMMITTER_NAME", "Branchyard"),
    ("GIT_COMMITTER_EMAIL", "branchyard@localhost"),
];

/// Why a transfer failed.
#[derive(Debug)]
pub enum Error {
    /// A git command on the host failed.
    Host(String),
    /// A step inside the actor failed.
    Guest(String),
    /// The host worktree changed while the harness ran in the actor, so the
    /// result cannot be applied without losing one side's changes.
    WorktreeChanged(String),
    /// The actor's `HEAD` does not descend from the commit that was sent:
    /// the harness rewrote history below it. Nothing was applied.
    Rewritten(String),
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Host(why) => write!(f, "on the host: {why}"),
            Error::Guest(why) => write!(f, "in the sandbox: {why}"),
            Error::WorktreeChanged(why) => write!(
                f,
                "the worktree changed on the host while the sandbox ran; not applying the \
                 sandbox's result: {why}"
            ),
            Error::Rewritten(why) => write!(
                f,
                "the sandbox rewrote history below the commit it was given; not applying its \
                 result: {why}"
            ),
            Error::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Error::Io(error)
    }
}

impl From<ProviderError> for Error {
    fn from(error: ProviderError) -> Self {
        Error::Guest(error.to_string())
    }
}

/// How the transfer reaches a sandbox: run a program there, and move
/// files and directory trees in and out.
pub trait Guest {
    /// Start `spec` in the sandbox, with stdin, stdout and stderr piped.
    fn exec(&self, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError>;
    /// Write `content` to `path` (its parent made) with permission bits
    /// `mode`.
    fn put_file(&self, path: &Path, mode: u32, content: &mut dyn Read) -> io::Result<()>;
    /// Copy `path` into `out`.
    fn get_file(&self, path: &Path, out: &mut dyn Write) -> io::Result<()>;
    /// Copy the host directory `from` to `to` in the sandbox.
    fn put_tree(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Copy the sandbox directory `from` into the host directory `to`,
    /// which must not exist; `NotFound` when `from` does not exist.
    fn get_tree(&self, from: &Path, to: &Path) -> io::Result<()>;
}

impl Guest for Endpoint {
    fn exec(&self, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError> {
        Endpoint::exec(self, spec).map(|p| Box::new(p) as Box<dyn Process>)
    }

    fn put_file(&self, path: &Path, mode: u32, content: &mut dyn Read) -> io::Result<()> {
        Endpoint::put_file(self, path, mode, content)
    }

    fn get_file(&self, path: &Path, out: &mut dyn Write) -> io::Result<()> {
        Endpoint::get_file(self, path, out)
    }

    fn put_tree(&self, from: &Path, to: &Path) -> io::Result<()> {
        Endpoint::put_tree(self, from, to)
    }

    fn get_tree(&self, from: &Path, to: &Path) -> io::Result<()> {
        Endpoint::get_tree(self, from, to)
    }
}

/// A [`Guest`] over one sandbox of any provider that can exec: files cross
/// as byte streams of `cat` and `tar` on the sandbox's side, through the
/// exec's standard input and output.
pub struct Exec<'a> {
    provider: &'a dyn SandboxProvider,
    sandbox: &'a str,
}

/// `$1` is the path, `$2` the mode in octal; the content is stdin.
const PUT_FILE: &str = r#"umask 077
case $1 in */*) mkdir -p -- "${1%/*}" || exit 1 ;; esac
cat >"$1.by-incoming" && chmod "$2" "$1.by-incoming" && mv -f "$1.by-incoming" "$1""#;

/// `$1` is the directory; a tar stream on stdin is unpacked into it.
const PUT_TREE: &str = r#"umask 077
mkdir -p -- "$1" && tar -x -f - -C "$1""#;

/// `$1` is the directory; its tar stream goes to stdout. Exit 3: missing.
const GET_TREE: &str = r#"[ -d "$1" ] || exit 3
tar -c -f - -C "$1" ."#;

impl<'a> Exec<'a> {
    pub fn new(provider: &'a dyn SandboxProvider, sandbox: &'a str) -> Exec<'a> {
        Exec { provider, sandbox }
    }

    /// `sh -c script sh args...` in the sandbox.
    fn sh(&self, script: &str, args: &[&str]) -> io::Result<Box<dyn Process>> {
        let spec = ExecSpec {
            argv: ["sh", "-c", script, "sh"]
                .iter()
                .chain(args)
                .map(|a| (*a).to_owned())
                .collect(),
            cwd: PathBuf::from("/"),
            env: BTreeMap::from([("LC_ALL".into(), "C".into())]),
        };
        self.provider
            .exec(self.sandbox, &spec)
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

/// Wait for `process`, whose stdin and stdout are already taken; its
/// stderr joins the error. Exit 3 of [`GET_TREE`] is `NotFound`.
fn finish(mut process: Box<dyn Process>, what: &str) -> io::Result<()> {
    let errors = process.take_stderr().map(|mut stderr| {
        thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        })
    });
    let status = process.wait()?;
    let stderr = errors.and_then(|e| e.join().ok()).unwrap_or_default();
    process.teardown();
    match (status.success(), status.code) {
        (true, _) => Ok(()),
        (false, Some(3)) if what.starts_with("get_tree") => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{what}: no such directory"),
        )),
        (false, _) => Err(io::Error::other(format!(
            "{what} failed with {status}: {}",
            stderr.trim()
        ))),
    }
}

/// Copy `from` into the exec's stdin, then close it.
fn pump(from: &mut dyn Read, to: Option<Box<dyn Write + Send>>) -> io::Result<()> {
    let mut to = to.ok_or_else(|| io::Error::other("the exec's stdin is not piped"))?;
    io::copy(from, &mut to)?;
    to.flush()
}

/// `tar` on this host, reading or writing a stream.
fn host_tar(args: &[&str], dir: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("tar");
    command.args(args).arg("-C").arg(dir).env("LC_ALL", "C");
    command
}

fn utf8(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| io::Error::other(format!("{} is not UTF-8", path.display())))
}

impl Guest for Exec<'_> {
    fn exec(&self, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError> {
        self.provider.exec(self.sandbox, spec)
    }

    fn put_file(&self, path: &Path, mode: u32, content: &mut dyn Read) -> io::Result<()> {
        let path = utf8(path)?;
        let mut process = self.sh(PUT_FILE, &[path, &format!("{mode:o}")])?;
        let sent = pump(content, process.take_stdin());
        drop(process.take_stdout());
        let finished = finish(process, &format!("put_file {path}"));
        finished.and(sent)
    }

    fn get_file(&self, path: &Path, out: &mut dyn Write) -> io::Result<()> {
        let path = utf8(path)?;
        let mut process = self.sh(r#"exec cat -- "$1""#, &[path])?;
        drop(process.take_stdin());
        let mut stdout = process
            .take_stdout()
            .ok_or_else(|| io::Error::other("the exec's stdout is not piped"))?;
        let copied = io::copy(&mut stdout, out).map(|_| ());
        drop(stdout);
        let finished = finish(process, &format!("get_file {path}"));
        finished.and(copied)
    }

    fn put_tree(&self, from: &Path, to: &Path) -> io::Result<()> {
        let to = utf8(to)?;
        let mut tar = host_tar(&["-c", "-f", "-"], from)
            .arg(".")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let mut process = match self.sh(PUT_TREE, &[to]) {
            Ok(process) => process,
            Err(error) => {
                let _ = tar.kill();
                let _ = tar.wait();
                return Err(error);
            }
        };
        let mut stream = tar.stdout.take().expect("piped");
        let sent = pump(&mut stream, process.take_stdin());
        drop(stream);
        drop(process.take_stdout());
        let finished = finish(process, &format!("put_tree {to}"));
        let packed = tar.wait_with_output()?;
        if !packed.status.success() {
            return Err(io::Error::other(format!(
                "tar of {} failed: {}",
                from.display(),
                String::from_utf8_lossy(&packed.stderr).trim()
            )));
        }
        finished.and(sent)
    }

    fn get_tree(&self, from: &Path, to: &Path) -> io::Result<()> {
        if to.symlink_metadata().is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", to.display()),
            ));
        }
        let from = utf8(from)?;
        let mut process = self.sh(GET_TREE, &[from])?;
        drop(process.take_stdin());
        let mut stdout = process
            .take_stdout()
            .ok_or_else(|| io::Error::other("the exec's stdout is not piped"))?;
        fs::create_dir_all(to)?;
        let unpacked = host_tar(&["-x", "-f", "-"], to)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut tar| {
                let mut sink = tar.stdin.take().expect("piped");
                let copied = io::copy(&mut stdout, &mut sink).map(|_| ());
                drop(sink);
                let out = tar.wait_with_output()?;
                copied?;
                match out.status.success() {
                    true => Ok(()),
                    false => Err(io::Error::other(format!(
                        "unpacking {from} failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    ))),
                }
            });
        drop(stdout);
        let finished = finish(process, &format!("get_tree {from}"));
        let done = finished.and(unpacked);
        if done.is_err() {
            let _ = fs::remove_dir_all(to);
        }
        done
    }
}

fn unique(what: &str) -> String {
    format!(
        "{what}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// A worktree pushed into an actor, and the staging repository that holds
/// its snapshot until [`pull`]. Dropping it deletes the staging repository.
#[derive(Debug)]
pub struct Pushed {
    /// The worktree's `HEAD` commit.
    pub base: String,
    /// The commit whose tree is the worktree's files as pushed.
    pub snapshot: String,
    /// Where the worktree is in the actor.
    pub guest: PathBuf,
    stage: Stage,
}

/// What [`pull`] applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pulled {
    /// The actor's snapshot commit (in the staging repository only).
    pub result: String,
    /// Whether any file differs from what was pushed.
    pub changed: bool,
    /// The commits the harness made, oldest first, now on the host branch.
    pub commits: Vec<String>,
}

/// A bare staging repository borrowing the host repository's objects.
#[derive(Debug)]
struct Stage {
    dir: PathBuf,
    /// The host repository's object directory.
    objects: PathBuf,
}

impl Stage {
    fn new(worktree: &Path, dir: Option<&Path>) -> Result<Stage, Error> {
        let common = host(
            worktree,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            &[],
        )?;
        let objects = PathBuf::from(common.trim()).join("objects");
        let dir = match dir {
            Some(dir) => dir.to_path_buf(),
            None => std::env::temp_dir().join(unique("branchyard-transfer")),
        };
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        let stage = Stage { dir, objects };
        host(&stage.dir, &["init", "-q", "--bare", "repo.git"], &[])?;
        fs::write(
            stage.git().join("objects/info/alternates"),
            format!("{}\n", stage.objects.display()),
        )?;
        Ok(stage)
    }

    fn git(&self) -> PathBuf {
        self.dir.join("repo.git")
    }

    /// Environment that makes git in the worktree read the host's objects
    /// but write new ones, and its index, into the stage.
    fn env(&self, index: &str) -> Vec<(OsString, OsString)> {
        vec![
            ("GIT_INDEX_FILE".into(), self.dir.join(index).into()),
            (
                "GIT_OBJECT_DIRECTORY".into(),
                self.git().join("objects").into(),
            ),
            (
                "GIT_ALTERNATE_OBJECT_DIRECTORIES".into(),
                self.objects.clone().into(),
            ),
        ]
    }

    /// Run git against the staging repository itself.
    fn run(&self, args: &[&str]) -> Result<String, Error> {
        let mut full = vec!["--git-dir"];
        let git = self.git();
        let git = git
            .to_str()
            .ok_or_else(|| Error::Host("non-UTF-8 temp path".into()))?;
        full.push(git);
        full.extend_from_slice(args);
        host(&self.dir, &full, &[])
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Run git on the host in `dir` and return stdout.
fn host(dir: &Path, args: &[&str], env: &[(OsString, OsString)]) -> Result<String, Error> {
    let (ok, stdout, stderr) = host_output(dir, args, env)?;
    match ok {
        true => Ok(stdout),
        false => Err(Error::Host(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim()
        ))),
    }
}

fn host_output(
    dir: &Path,
    args: &[&str],
    env: &[(OsString, OsString)],
) -> Result<(bool, String, String), Error> {
    // branchyard-workspace's `Git` scrubs the variables that would point
    // git elsewhere; `env` then names this transfer's own index and object
    // store.
    let mut git = Git::new(dir).args(args);
    for (key, value) in IDENTITY {
        git = git.env(key, value);
    }
    for (key, value) in env {
        git = git.env(key, value);
    }
    let (out, _) = git.output().map_err(|e| Error::Host(e.to_string()))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// Snapshot the files of the worktree at `dir` on top of `HEAD` using
/// `env`'s index and object store. Returns the commit.
fn snapshot_host(dir: &Path, env: &[(OsString, OsString)], message: &str) -> Result<String, Error> {
    host(dir, &["read-tree", "HEAD"], env)?;
    host(dir, &["add", "--all", "--", "."], env)?;
    let tree = host(dir, &["write-tree"], env)?;
    let commit = host(
        dir,
        &[
            "commit-tree",
            "--no-gpg-sign",
            tree.trim(),
            "-p",
            "HEAD",
            "-m",
            message,
        ],
        env,
    )?;
    Ok(commit.trim().to_owned())
}

/// Run git in the actor in `cwd` and return stdout.
fn guest(
    endpoint: &dyn Guest,
    cwd: &Path,
    args: &[&str],
    env: &[(&str, String)],
) -> Result<String, Error> {
    let mut vars: BTreeMap<OsString, OsString> = IDENTITY
        .iter()
        .map(|(n, v)| (OsString::from(n), OsString::from(v)))
        .collect();
    vars.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    vars.insert("LC_ALL".into(), "C".into());
    for (name, value) in env {
        vars.insert(name.into(), value.into());
    }
    let spec = ExecSpec {
        argv: std::iter::once("git")
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect(),
        cwd: cwd.to_path_buf(),
        env: vars,
    };
    let mut process = endpoint
        .exec(&spec)
        .map_err(|e| Error::Guest(format!("could not run git {}: {e}", args.join(" "))))?;
    drop(process.take_stdin());
    let mut stderr = process.take_stderr().expect("stderr is piped");
    let errors = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let mut stdout = String::new();
    let _ = process
        .take_stdout()
        .expect("stdout is piped")
        .read_to_string(&mut stdout);
    let status = process.wait()?;
    let stderr = errors.join().unwrap_or_default();
    process.teardown();
    match status.success() {
        true => Ok(stdout),
        false => Err(Error::Guest(format!(
            "git {} failed with {status}: {}",
            args.join(" "),
            stderr.trim()
        ))),
    }
}

fn path_str(path: &Path) -> Result<&str, Error> {
    path.to_str()
        .ok_or_else(|| Error::Guest(format!("{} is not UTF-8", path.display())))
}

/// Recreate the host `worktree` at `guest_dir` in the actor, which must not
/// hold a repository yet. The staging repository is a new directory under
/// the system's temporary directory; see [`push_staged`] to choose it.
pub fn push(endpoint: &dyn Guest, worktree: &Path, guest_dir: &Path) -> Result<Pushed, Error> {
    push_staged(endpoint, worktree, guest_dir, None)
}

/// [`push`], staging in `stage` (replaced if it exists), so that whoever
/// recovers from a crash knows what to delete.
pub fn push_staged(
    endpoint: &dyn Guest,
    worktree: &Path,
    guest_dir: &Path,
    stage: Option<&Path>,
) -> Result<Pushed, Error> {
    if !guest_dir.is_absolute() {
        return Err(Error::Guest(format!(
            "{} is not an absolute path",
            guest_dir.display()
        )));
    }
    let stage = Stage::new(worktree, stage)?;
    let base = host(worktree, &["rev-parse", "--verify", "HEAD^{commit}"], &[])?
        .trim()
        .to_owned();
    let snapshot = snapshot_host(
        worktree,
        &stage.env("push.index"),
        "branchyard: worktree as sent to a sandbox",
    )?;
    stage.run(&["update-ref", "refs/branchyard/base", &base])?;
    stage.run(&["update-ref", "refs/branchyard/snapshot", &snapshot])?;
    let bundle = stage.dir.join("in.bundle");
    stage.run(&[
        "bundle",
        "create",
        "-q",
        path_str(&bundle)?,
        "refs/branchyard/base",
        "refs/branchyard/snapshot",
    ])?;
    let branch = host(worktree, &["symbolic-ref", "-q", "--short", "HEAD"], &[])
        .map(|b| b.trim().to_owned())
        .ok()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "branchyard".into());

    let dir = path_str(guest_dir)?;
    guest(endpoint, Path::new("/"), &["init", "-q", dir], &[])?;
    if guest(
        endpoint,
        guest_dir,
        &["rev-parse", "-q", "--verify", "HEAD"],
        &[],
    )
    .is_ok()
    {
        return Err(Error::Guest(format!(
            "{dir} already holds a repository with commits"
        )));
    }
    let incoming = guest_dir.join(".git/branchyard/in.bundle");
    endpoint.put_file(&incoming, 0o600, &mut fs::File::open(&bundle)?)?;
    let incoming = path_str(&incoming)?;
    for args in [
        &["config", "user.name", "Branchyard sandbox"][..],
        &["config", "user.email", "sandbox@branchyard.invalid"],
        &[
            "fetch",
            "-q",
            "--no-tags",
            "--no-write-fetch-head",
            incoming,
            "refs/branchyard/base:refs/branchyard/base",
            "refs/branchyard/snapshot:refs/branchyard/snapshot",
        ],
        &[
            "checkout",
            "-q",
            "-f",
            "-B",
            &branch,
            "refs/branchyard/snapshot",
        ],
        &["reset", "-q", "--soft", "refs/branchyard/base"],
        &["reset", "-q"],
    ] {
        guest(endpoint, guest_dir, args, &[])?;
    }
    Ok(Pushed {
        base,
        snapshot,
        guest: guest_dir.to_path_buf(),
        stage,
    })
}

/// The [`Pushed`] a stopped process's [`push_staged`] of `worktree` to
/// `guest_dir` left in `stage`, so that another process can [`pull`] the
/// result. Fails unless the staging repository holds both of the push's
/// snapshot refs. The staging directory is deleted when the result is
/// dropped, as for a push, or at once when it holds no push.
pub fn reopen_staged(worktree: &Path, guest_dir: &Path, stage: &Path) -> Result<Pushed, Error> {
    let common = host(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        &[],
    )?;
    let stage = Stage {
        dir: stage.to_path_buf(),
        objects: PathBuf::from(common.trim()).join("objects"),
    };
    let read = |name: &str| -> Result<String, Error> {
        let reference = format!("refs/branchyard/{name}^{{commit}}");
        Ok(stage
            .run(&["rev-parse", "--verify", "-q", &reference])
            .map_err(|_| Error::Host(format!("{} holds no {name} of a push", stage.dir.display())))?
            .trim()
            .to_owned())
    };
    let (base, snapshot) = (read("base")?, read("snapshot")?);
    Ok(Pushed {
        base,
        snapshot,
        guest: guest_dir.to_path_buf(),
        stage,
    })
}

/// Bring the actor's worktree back and apply it to the host `worktree`,
/// whose `HEAD` must still be the commit [`push`] sent and whose files
/// must still be exactly what it sent. The harness's commits move the
/// host branch; its uncommitted changes land in the working tree.
pub fn pull(endpoint: &dyn Guest, pushed: &Pushed, worktree: &Path) -> Result<Pulled, Error> {
    let dir = &pushed.guest;
    let meta = dir.join(".git/branchyard");
    let index = path_str(&meta.join("result.index"))?.to_owned();
    let outgoing = meta.join("out.bundle");
    let env = [("GIT_INDEX_FILE", index)];
    guest(endpoint, dir, &["read-tree", "HEAD"], &env)?;
    guest(endpoint, dir, &["add", "--all", "--", "."], &env)?;
    let tree = guest(endpoint, dir, &["write-tree"], &env)?;
    let result = guest(
        endpoint,
        dir,
        &[
            "commit-tree",
            "--no-gpg-sign",
            tree.trim(),
            "-p",
            "HEAD",
            "-m",
            "branchyard: sandbox result",
        ],
        &[],
    )?;
    let result = result.trim().to_owned();
    guest(
        endpoint,
        dir,
        &["update-ref", "refs/branchyard/result", &result],
        &[],
    )?;
    guest(
        endpoint,
        dir,
        &[
            "bundle",
            "create",
            "-q",
            path_str(&outgoing)?,
            "refs/branchyard/result",
            "^refs/branchyard/base",
        ],
        &[],
    )?;

    let stage = &pushed.stage;
    let bundle = stage.dir.join("out.bundle");
    endpoint.get_file(&outgoing, &mut fs::File::create(&bundle)?)?;
    stage.run(&[
        "-c",
        "transfer.fsckObjects=true",
        "fetch",
        "-q",
        "--no-tags",
        "--no-write-fetch-head",
        path_str(&bundle)?,
        "refs/branchyard/result:refs/branchyard/result",
    ])?;
    let fetched = stage.run(&["rev-parse", "--verify", "refs/branchyard/result^{commit}"])?;
    if fetched.trim() != result {
        return Err(Error::Guest(format!(
            "the bundle holds {} rather than the reported result {result}",
            fetched.trim()
        )));
    }
    // The result is the actor's files on top of the actor's HEAD, which the
    // host reads from the fetched objects rather than from the actor.
    let head = stage
        .run(&["rev-parse", "--verify", &format!("{result}^1^{{commit}}")])
        .map_err(|_| Error::Guest(format!("the result {result} has no parent")))?
        .trim()
        .to_owned();
    if stage
        .run(&["rev-parse", "--verify", "-q", &format!("{result}^2")])
        .is_ok()
    {
        return Err(Error::Guest(format!("the result {result} is a merge")));
    }
    let commits = match head == pushed.base {
        true => Vec::new(),
        false => {
            let (descends, _, _) = host_output(
                &stage.dir,
                &[
                    "--git-dir",
                    path_str(&stage.git())?,
                    "merge-base",
                    "--is-ancestor",
                    &pushed.base,
                    &head,
                ],
                &[],
            )?;
            if !descends {
                return Err(Error::Rewritten(format!(
                    "the sandbox's HEAD {head} does not descend from {}",
                    pushed.base
                )));
            }
            stage
                .run(&[
                    "rev-list",
                    "--reverse",
                    "--topo-order",
                    &format!("{}..{head}", pushed.base),
                ])?
                .lines()
                .map(str::to_owned)
                .collect()
        }
    };

    let now = host(worktree, &["rev-parse", "--verify", "HEAD^{commit}"], &[])?;
    if now.trim() != pushed.base {
        return Err(Error::WorktreeChanged(format!(
            "HEAD moved from {} to {}",
            pushed.base,
            now.trim()
        )));
    }
    let env = stage.env("pull.index");
    host(worktree, &["read-tree", &pushed.snapshot], &env)?;
    let (clean, _, stderr) = host_output(worktree, &["update-index", "--refresh"], &env)?;
    if !clean {
        return Err(Error::WorktreeChanged(stderr.trim().to_owned()));
    }
    if !commits.is_empty() {
        // The commits enter the host's object store now, checked again;
        // nothing refers to them until the branch moves below.
        stage.run(&["update-ref", "refs/branchyard/head", &head])?;
        host(
            worktree,
            &[
                "-c",
                "transfer.fsckObjects=true",
                "-c",
                NO_HOOKS,
                "fetch",
                "-q",
                "--no-tags",
                "--no-write-fetch-head",
                path_str(&stage.git())?,
                "refs/branchyard/head",
            ],
            &[],
        )?;
    }
    let before = host(
        worktree,
        &["rev-parse", &format!("{}^{{tree}}", pushed.snapshot)],
        &env,
    )?;
    let after = host(
        worktree,
        &["rev-parse", &format!("{result}^{{tree}}")],
        &env,
    )?;
    let changed = before.trim() != after.trim();
    if changed {
        host(
            worktree,
            &["read-tree", "-m", "-u", before.trim(), after.trim()],
            &env,
        )
        .map_err(|e| match e {
            Error::Host(why) => Error::WorktreeChanged(why),
            other => other,
        })?;
    }
    if !commits.is_empty() {
        if let Err(error) = advance(worktree, &pushed.base, &head) {
            // Put the files back as they were, so nothing half-applied is
            // left for the engine to record.
            if changed {
                let _ = host(
                    worktree,
                    &["read-tree", "-m", "-u", after.trim(), before.trim()],
                    &env,
                );
            }
            return Err(error);
        }
    }
    Ok(Pulled {
        result,
        changed,
        commits,
    })
}

/// Disables hooks for git commands that write the host repository.
const NO_HOOKS: &str = "core.hooksPath=/dev/null";

/// Move the worktree's branch (or detached `HEAD`) from `base` to `head`,
/// only if it is still at `base`, and its index to `head`, leaving the
/// working files as they are.
fn advance(worktree: &Path, base: &str, head: &str) -> Result<(), Error> {
    let message = "branchyard: commits made in a sandbox";
    let branch = host(worktree, &["symbolic-ref", "-q", "HEAD"], &[])
        .map(|b| b.trim().to_owned())
        .ok()
        .filter(|b| !b.is_empty());
    let moved = match &branch {
        Some(branch) => host(
            worktree,
            &[
                "-c",
                NO_HOOKS,
                "update-ref",
                "-m",
                message,
                branch,
                head,
                base,
            ],
            &[],
        ),
        None => host(
            worktree,
            &[
                "-c",
                NO_HOOKS,
                "update-ref",
                "--no-deref",
                "-m",
                message,
                "HEAD",
                head,
                base,
            ],
            &[],
        ),
    };
    moved.map_err(|e| match e {
        Error::Host(why) => Error::WorktreeChanged(why),
        other => other,
    })?;
    host(worktree, &["read-tree", head], &[])?;
    // Refresh stat information; files that differ from `head` are the
    // uncommitted changes, so a non-zero exit is expected.
    let _ = host_output(worktree, &["update-index", "-q", "--refresh"], &[]);
    Ok(())
}

/// Run in the actor by [`clear_for_push`]: `sh -c CLEAR sh WORKDIR HOME`.
/// In the worktree's repository, every file git tracks or would track
/// (untracked and not ignored) is removed, then the repository itself;
/// ignored files, such as what a workspace setup installed, stay. The home
/// is emptied.
pub const CLEAR: &str = r#"w=$1 h=$2
if [ -d "$w/.git" ]; then
  cd "$w" || exit 1
  git ls-files -c -o --exclude-standard | while IFS= read -r f; do rm -f -- "$f"; done
  rm -rf .git
fi
if [ -n "$h" ] && [ "$h" != / ]; then rm -rf "$h"; fi
exit 0"#;

/// Make a resumed or branched actor ready for [`push`] again: its old
/// worktree repository and the files git sees in it are removed (ignored
/// files stay), and its home is removed. The actor needs `sh` and `git`.
pub fn clear_for_push(endpoint: &dyn Guest, workdir: &Path, home: &Path) -> Result<(), Error> {
    clear(endpoint, CLEAR, workdir, path_str(home)?)
}

/// Make a machine that may be reused, though its sandbox is new (a
/// recipe's static machine), ready for [`push`]: only a worktree
/// repository an earlier push made (it has `.git/branchyard`) is cleared
/// as [`clear_for_push`] clears it; any other repository there is left,
/// and the push then refuses it. The home is left: what is sent is
/// written over it.
pub fn clear_previous_push(endpoint: &dyn Guest, workdir: &Path) -> Result<(), Error> {
    let script = format!("[ -d \"$1/.git/branchyard\" ] || exit 0\n{CLEAR}");
    clear(endpoint, &script, workdir, "")
}

fn clear(endpoint: &dyn Guest, script: &str, workdir: &Path, home: &str) -> Result<(), Error> {
    let spec = ExecSpec {
        argv: vec![
            "sh".into(),
            "-c".into(),
            script.into(),
            "sh".into(),
            path_str(workdir)?.to_owned(),
            home.to_owned(),
        ],
        cwd: PathBuf::from("/"),
        env: BTreeMap::from([("LC_ALL".into(), "C".into())]),
    };
    let mut process = endpoint
        .exec(&spec)
        .map_err(|e| Error::Guest(format!("could not clear {}: {e}", workdir.display())))?;
    drop(process.take_stdin());
    let mut stderr = String::new();
    let _ = process
        .take_stderr()
        .expect("stderr is piped")
        .read_to_string(&mut stderr);
    let status = process.wait()?;
    process.teardown();
    match status.success() {
        true => Ok(()),
        false => Err(Error::Guest(format!(
            "clearing {} failed with {status}: {}",
            workdir.display(),
            stderr.trim()
        ))),
    }
}

/// Copy the host directory `from` to `to` in the actor, if it exists.
pub fn push_tree(endpoint: &dyn Guest, from: &Path, to: &Path) -> Result<(), Error> {
    if !from.is_dir() {
        return Ok(());
    }
    Ok(endpoint.put_tree(from, to)?)
}

/// Replace the host directory `to` with the actor's `from`, if the actor
/// has it. The new tree is received beside `to` and swapped in, so a failed
/// transfer leaves `to` as it was.
pub fn pull_tree(endpoint: &dyn Guest, from: &Path, to: &Path) -> Result<(), Error> {
    let name = to
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Host(format!("{} has no file name", to.display())))?;
    let incoming = to.with_file_name(unique(&format!(".{name}.incoming")));
    match endpoint.get_tree(from, &incoming) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let _ = fs::remove_dir_all(&incoming);
            return Ok(());
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&incoming);
            return Err(error.into());
        }
    }
    let old = to.with_file_name(unique(&format!(".{name}.old")));
    if to.symlink_metadata().is_ok() {
        fs::rename(to, &old)?;
    }
    if let Err(error) = fs::rename(&incoming, to) {
        let _ = fs::rename(&old, to);
        return Err(error.into());
    }
    let _ = fs::remove_dir_all(&old);
    Ok(())
}

#[cfg(test)]
mod tests {
    //! [`push`] and [`pull`] against a sandbox that is a directory of this
    //! host: [`HostProvider`] runs each exec as a local process, so the
    //! real [`Exec`] guest carries real git bundles and tar streams between
    //! two repositories on disk. The same steps through a Substrate actor's
    //! bridge are in `tests/transfer.rs`. Requires `git`, `sh` and `tar`.
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Child, Command, Stdio};

    use branchyard_sandbox::{Capabilities, ExitStatus, SandboxInfo, SandboxSpec, SandboxState};

    use super::*;

    /// Runs every exec as a process on this host.
    struct HostProvider {
        broken: bool,
    }

    struct HostProcess {
        child: Child,
    }

    impl Process for HostProcess {
        fn id(&self) -> String {
            self.child.id().to_string()
        }
        fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
            self.child
                .stdin
                .take()
                .map(|s| Box::new(s) as Box<dyn Write + Send>)
        }
        fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
            self.child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>)
        }
        fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
            self.child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>)
        }
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(self.child.try_wait()?.map(status))
        }
        fn wait(&mut self) -> io::Result<ExitStatus> {
            Ok(status(self.child.wait()?))
        }
        fn kill(&mut self) -> io::Result<()> {
            self.child.kill()
        }
        fn teardown(&mut self) -> Vec<String> {
            Vec::new()
        }
    }

    fn status(s: std::process::ExitStatus) -> ExitStatus {
        ExitStatus {
            code: s.code(),
            signal: s.signal(),
        }
    }

    impl SandboxProvider for HostProvider {
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        fn ensure(&self, spec: &SandboxSpec) -> Result<SandboxInfo, ProviderError> {
            self.inspect(&spec.name).map(|i| i.expect("always there"))
        }
        fn inspect(&self, name: &str) -> Result<Option<SandboxInfo>, ProviderError> {
            Ok(Some(SandboxInfo {
                name: name.into(),
                state: SandboxState::Running,
            }))
        }
        fn exec(&self, _: &str, spec: &ExecSpec) -> Result<Box<dyn Process>, ProviderError> {
            if self.broken {
                return Err(ProviderError::Runtime("the sandbox is gone".into()));
            }
            let mut command = Command::new(&spec.argv[0]);
            command
                .args(&spec.argv[1..])
                .current_dir(&spec.cwd)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .envs(&spec.env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            Ok(Box::new(HostProcess {
                child: command.spawn()?,
            }))
        }
        fn stop(&self, _: &str) -> Result<(), ProviderError> {
            Ok(())
        }
        fn destroy(&self, _: &str) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    const PROVIDER: HostProvider = HostProvider { broken: false };

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// A host repository with committed files of each kind and an ignored
    /// directory, and a remote URL that must never reach the sandbox.
    fn repository(dir: &Path) -> PathBuf {
        let root = dir.join("repo");
        fs::create_dir_all(root.join("src")).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "user.email", "test@localhost"]);
        git(
            &root,
            &[
                "config",
                "remote.origin.url",
                "https://secret@example.com/r.git",
            ],
        );
        fs::write(root.join("a.txt"), "one\n").unwrap();
        fs::write(root.join("src/b.txt"), "two\n").unwrap();
        fs::write(root.join("tool.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(root.join("tool.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        root
    }

    struct Setup {
        _dir: tempfile::TempDir,
        root: PathBuf,
        guest: PathBuf,
    }

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let root = repository(dir.path());
        let guest = dir.path().join("sandbox/work");
        Setup {
            _dir: dir,
            root,
            guest,
        }
    }

    fn sandbox_git(s: &Setup, args: &[&str]) -> String {
        git(&s.guest, args)
    }

    #[test]
    fn a_push_recreates_the_worktree_in_the_sandbox_and_a_pull_brings_the_result_back() {
        let s = setup();
        let exec = Exec::new(&PROVIDER, "box");
        // Uncommitted state on the host that must reach the sandbox.
        fs::write(s.root.join("a.txt"), "one, edited on the host\n").unwrap();
        fs::write(s.root.join("untracked.txt"), "untracked\n").unwrap();
        fs::create_dir_all(s.root.join("ignored")).unwrap();
        fs::write(s.root.join("ignored/cache"), "never sent\n").unwrap();
        let head = git(&s.root, &["rev-parse", "HEAD"]);

        let pushed = push(&exec, &s.root, &s.guest).unwrap();
        assert_eq!(pushed.base, head);
        assert_eq!(pushed.guest, s.guest);
        // The files are as on the host, HEAD is the host's commit, the index
        // is at HEAD (so the edits are working-tree changes), and the
        // ignored file and the host's configuration stay behind.
        assert_eq!(sandbox_git(&s, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            fs::read_to_string(s.guest.join("a.txt")).unwrap(),
            "one, edited on the host\n"
        );
        assert_eq!(
            fs::read_to_string(s.guest.join("untracked.txt")).unwrap(),
            "untracked\n"
        );
        assert!(!s.guest.join("ignored").exists());
        assert_eq!(
            fs::metadata(s.guest.join("tool.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert!(!sandbox_git(&s, &["config", "--local", "--list"]).contains("secret"));
        let status = sandbox_git(&s, &["status", "--porcelain"]);
        assert!(
            status.contains("M a.txt") && status.contains("?? untracked.txt"),
            "{status}"
        );

        // The harness edits, adds, deletes, links and commits one change.
        fs::write(s.guest.join("src/b.txt"), "two, committed in the sandbox\n").unwrap();
        sandbox_git(&s, &["commit", "-q", "-am", "harness commit"]);
        fs::write(s.guest.join("new.txt"), "new\n").unwrap();
        fs::remove_file(s.guest.join("untracked.txt")).unwrap();
        symlink("a.txt", s.guest.join("link")).unwrap();
        let sandbox_head = sandbox_git(&s, &["rev-parse", "HEAD"]);

        let pulled = pull(&exec, &pushed, &s.root).unwrap();
        assert!(pulled.changed);
        assert_eq!(pulled.commits, std::slice::from_ref(&sandbox_head));
        // The host branch moved to the sandbox's commit, with its message.
        assert_eq!(git(&s.root, &["rev-parse", "HEAD"]), sandbox_head);
        assert_eq!(
            git(&s.root, &["log", "-1", "--format=%s"]),
            "harness commit"
        );
        assert_eq!(
            fs::read_to_string(s.root.join("src/b.txt")).unwrap(),
            "two, committed in the sandbox\n"
        );
        assert_eq!(fs::read_to_string(s.root.join("new.txt")).unwrap(), "new\n");
        assert!(!s.root.join("untracked.txt").exists());
        assert_eq!(
            fs::read_link(s.root.join("link")).unwrap(),
            Path::new("a.txt")
        );
        assert!(
            s.root.join("ignored/cache").exists(),
            "ignored files are never touched"
        );
        // What was not committed is a working-tree change on top.
        let status = git(&s.root, &["status", "--porcelain"]);
        assert!(
            status.contains("new.txt") && !status.contains("src/b.txt"),
            "{status}"
        );
    }

    #[test]
    fn a_pull_with_no_changes_touches_nothing_on_the_host() {
        let s = setup();
        let exec = Exec::new(&PROVIDER, "box");
        let refs = git(&s.root, &["for-each-ref"]);
        let pushed = push(&exec, &s.root, &s.guest).unwrap();
        let pulled = pull(&exec, &pushed, &s.root).unwrap();
        assert!(!pulled.changed);
        assert!(pulled.commits.is_empty());
        assert_eq!(git(&s.root, &["for-each-ref"]), refs);
        assert_eq!(git(&s.root, &["status", "--porcelain"]), "");
    }

    #[test]
    fn uncommitted_changes_alone_are_applied_without_moving_the_branch() {
        let s = setup();
        let exec = Exec::new(&PROVIDER, "box");
        let head = git(&s.root, &["rev-parse", "HEAD"]);
        let pushed = push(&exec, &s.root, &s.guest).unwrap();
        fs::write(s.guest.join("a.txt"), "edited in the sandbox\n").unwrap();
        let pulled = pull(&exec, &pushed, &s.root).unwrap();
        assert!(pulled.changed && pulled.commits.is_empty());
        assert_eq!(git(&s.root, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            fs::read_to_string(s.root.join("a.txt")).unwrap(),
            "edited in the sandbox\n"
        );
    }

    #[test]
    fn a_pull_refuses_when_the_host_branch_moved_meanwhile() {
        let s = setup();
        let exec = Exec::new(&PROVIDER, "box");
        let pushed = push(&exec, &s.root, &s.guest).unwrap();
        git(&s.root, &["commit", "-q", "--allow-empty", "-m", "moved"]);
        let err = pull(&exec, &pushed, &s.root).unwrap_err();
        assert!(
            matches!(&err, Error::WorktreeChanged(why) if why.contains("HEAD moved")),
            "{err}"
        );
    }

    #[test]
    fn a_pull_refuses_when_a_host_file_changed_meanwhile() {
        let s = setup();
        let exec = Exec::new(&PROVIDER, "box");
        let pushed = push(&exec, &s.root, &s.guest).unwrap();
        fs::write(s.guest.join("a.txt"), "sandbox edit\n").unwrap();
        fs::write(s.root.join("a.txt"), "host edit, after the push\n").unwrap();
        let err = pull(&exec, &pushed, &s.root).unwrap_err();
        assert!(matches!(err, Error::WorktreeChanged(_)), "{err}");
        assert_eq!(
            fs::read_to_string(s.root.join("a.txt")).unwrap(),
            "host edit, after the push\n",
            "the host's own change is not overwritten"
        );
    }

    #[test]
    fn history_rewritten_below_the_base_is_refused() {
        let s = setup();
        let exec = Exec::new(&PROVIDER, "box");
        let refs = git(&s.root, &["for-each-ref"]);
        let pushed = push(&exec, &s.root, &s.guest).unwrap();
        sandbox_git(&s, &["checkout", "-q", "--orphan", "rewritten"]);
        sandbox_git(&s, &["commit", "-q", "-m", "a new root"]);
        let err = pull(&exec, &pushed, &s.root).unwrap_err();
        assert!(matches!(err, Error::Rewritten(_)), "{err}");
        assert_eq!(git(&s.root, &["for-each-ref"]), refs);
    }

    #[test]
    fn a_push_needs_an_absolute_path_and_an_empty_repository() {
        let s = setup();
        let exec = Exec::new(&PROVIDER, "box");
        let err = push(&exec, &s.root, Path::new("relative/dir")).unwrap_err();
        assert!(
            matches!(&err, Error::Guest(why) if why.contains("not an absolute path")),
            "{err}"
        );
        let _first = push(&exec, &s.root, &s.guest).unwrap();
        let err = push(&exec, &s.root, &s.guest).unwrap_err();
        assert!(
            matches!(&err, Error::Guest(why) if why.contains("already holds a repository")),
            "{err}"
        );
    }

    #[test]
    fn a_broken_sandbox_is_a_guest_error() {
        let s = setup();
        let broken = HostProvider { broken: true };
        let err = push(&Exec::new(&broken, "box"), &s.root, &s.guest).unwrap_err();
        assert!(
            matches!(&err, Error::Guest(why) if why.contains("the sandbox is gone")),
            "{err}"
        );
    }

    #[test]
    fn a_staged_push_can_be_reopened_by_another_process() {
        let s = setup();
        let exec = Exec::new(&PROVIDER, "box");
        let stage = s.root.parent().unwrap().join("stage");
        let pushed = push_staged(&exec, &s.root, &s.guest, Some(&stage)).unwrap();
        let (base, snapshot) = (pushed.base.clone(), pushed.snapshot.clone());
        // The first process dies without dropping its stage.
        std::mem::forget(pushed);
        let reopened = reopen_staged(&s.root, &s.guest, &stage).unwrap();
        assert_eq!(
            (reopened.base.as_str(), reopened.snapshot.as_str()),
            (base.as_str(), snapshot.as_str())
        );
        fs::write(s.guest.join("late.txt"), "late\n").unwrap();
        let pulled = pull(&exec, &reopened, &s.root).unwrap();
        assert!(pulled.changed);
        assert!(s.root.join("late.txt").exists());
        drop(reopened);
        assert!(
            !stage.exists(),
            "dropping the push deletes its staging repository"
        );
    }

    #[test]
    fn reopening_a_stage_that_holds_no_push_fails() {
        let s = setup();
        let empty = s.root.parent().unwrap().join("empty-stage");
        fs::create_dir_all(&empty).unwrap();
        let err = reopen_staged(&s.root, &s.guest, &empty).unwrap_err();
        assert!(
            matches!(&err, Error::Host(why) if why.contains("holds no base")),
            "{err}"
        );
    }

    #[test]
    fn clearing_removes_what_git_sees_and_the_home_but_keeps_ignored_files() {
        let s = setup();
        let exec = Exec::new(&PROVIDER, "box");
        fs::create_dir_all(s.root.join("ignored")).unwrap();
        let _pushed = push(&exec, &s.root, &s.guest).unwrap();
        fs::create_dir_all(s.guest.join("ignored")).unwrap();
        fs::write(s.guest.join("ignored/installed"), "dependency\n").unwrap();
        fs::write(s.guest.join("scratch.txt"), "untracked\n").unwrap();
        let home = s.guest.parent().unwrap().join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("token"), "t").unwrap();

        clear_for_push(&exec, &s.guest, &home).unwrap();
        assert!(!s.guest.join(".git").exists());
        assert!(!s.guest.join("a.txt").exists());
        assert!(!s.guest.join("scratch.txt").exists());
        assert!(s.guest.join("ignored/installed").exists());
        assert!(!home.exists());
    }

    #[test]
    fn clearing_a_previous_push_leaves_a_repository_that_is_not_one() {
        let s = setup();
        let exec = Exec::new(&PROVIDER, "box");
        // Somebody else's repository: no .git/branchyard.
        fs::create_dir_all(&s.guest).unwrap();
        git(&s.guest, &["init", "-q"]);
        fs::write(s.guest.join("theirs.txt"), "mine\n").unwrap();
        clear_previous_push(&exec, &s.guest).unwrap();
        assert!(s.guest.join("theirs.txt").exists());
        assert!(s.guest.join(".git").exists());
        // A push of ours is cleared.
        let ours = s.guest.parent().unwrap().join("ours");
        let _pushed = push(&exec, &s.root, &ours).unwrap();
        clear_previous_push(&exec, &ours).unwrap();
        assert!(!ours.join(".git").exists());
        assert!(!ours.join("a.txt").exists());
    }

    #[test]
    fn a_clear_in_an_unreachable_sandbox_is_a_guest_error() {
        let s = setup();
        let err = clear_for_push(
            &Exec::new(&HostProvider { broken: true }, "box"),
            &s.guest,
            Path::new("/tmp/h"),
        )
        .unwrap_err();
        assert!(
            matches!(&err, Error::Guest(why) if why.contains("could not clear")),
            "{err}"
        );
    }

    #[test]
    fn files_and_trees_cross_as_streams() {
        let s = setup();
        let exec = Exec::new(&PROVIDER, "box");
        let dir = s.root.parent().unwrap();
        // A file, with its mode, into a directory that does not exist yet.
        let target = dir.join("sandbox/deep/file.bin");
        Guest::put_file(&exec, &target, 0o600, &mut &b"payload"[..]).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"payload");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut back = Vec::new();
        Guest::get_file(&exec, &target, &mut back).unwrap();
        assert_eq!(back, b"payload");
        let err = Guest::get_file(&exec, &dir.join("absent"), &mut Vec::new()).unwrap_err();
        assert!(err.to_string().contains("get_file"), "{err}");

        // A tree out and back in.
        let home = dir.join("home");
        fs::create_dir_all(home.join("sub")).unwrap();
        fs::write(home.join("sub/x"), "x").unwrap();
        let theirs = dir.join("sandbox/home");
        push_tree(&exec, &home, &theirs).unwrap();
        assert_eq!(fs::read_to_string(theirs.join("sub/x")).unwrap(), "x");
        fs::write(theirs.join("sub/y"), "y").unwrap();
        pull_tree(&exec, &theirs, &home).unwrap();
        assert_eq!(fs::read_to_string(home.join("sub/y")).unwrap(), "y");
        let leftovers: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".incoming") || n.contains(".old"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_tree_that_is_absent_on_either_side_is_skipped() {
        let s = setup();
        let exec = Exec::new(&PROVIDER, "box");
        let dir = s.root.parent().unwrap();
        // Nothing on the host to send.
        push_tree(&exec, &dir.join("no-such-host-dir"), &dir.join("sandbox/x")).unwrap();
        assert!(!dir.join("sandbox/x").exists());
        // Nothing in the sandbox to bring back: the host's tree stays.
        let home = dir.join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("keep"), "k").unwrap();
        pull_tree(&exec, &dir.join("no-such-sandbox-dir"), &home).unwrap();
        assert_eq!(fs::read_to_string(home.join("keep")).unwrap(), "k");
    }

    #[test]
    fn a_pull_tree_target_needs_a_file_name_and_may_not_already_be_received() {
        let s = setup();
        let exec = Exec::new(&PROVIDER, "box");
        let err = pull_tree(&exec, Path::new("/tmp"), Path::new("/")).unwrap_err();
        assert!(matches!(err, Error::Host(_)), "{err}");
        let existing = s.root.parent().unwrap().join("existing");
        fs::create_dir_all(&existing).unwrap();
        let err = Guest::get_tree(&exec, Path::new("/tmp"), &existing).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn errors_say_which_side_failed() {
        assert_eq!(Error::Host("x".into()).to_string(), "on the host: x");
        assert_eq!(Error::Guest("y".into()).to_string(), "in the sandbox: y");
        assert!(Error::WorktreeChanged("z".into())
            .to_string()
            .contains("changed on the host"));
        assert!(Error::Rewritten("w".into())
            .to_string()
            .contains("rewrote history"));
        let io: Error = io::Error::other("disk").into();
        assert_eq!(io.to_string(), "disk");
        let provider: Error = ProviderError::Runtime("down".into()).into();
        assert!(matches!(provider, Error::Guest(_)));
    }
}
