//! Code in and out of an actor, which has no host mounts: git bundles and
//! directory trees carried by the bridge.
//!
//! [`push`] snapshots the worktree (its `HEAD` commit plus every tracked and
//! untracked, non-ignored file, as a commit on top of `HEAD`), bundles both
//! commits and their history, and recreates the worktree in a fresh
//! repository in the actor: `HEAD` at the same commit, the working files as
//! they were on the host, the index at `HEAD`. [`pull`] snapshots the
//! actor's working files the same way, brings that one commit back in a
//! bundle, and applies the difference between the two snapshots to the
//! host worktree's files: additions, changes, deletions, modes and
//! symbolic links. The host's index, refs and branch are not touched; the
//! engine's own snapshot then records the candidate as it would for a local
//! harness. Commits the harness made inside the actor are flattened into
//! that difference.
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
//! `transfer.fsckObjects`, and are deleted with it. The host repository's
//! refs and object store are never written.
//!
//! The actor needs `git` on its `PATH`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use branchyard_bridge::Endpoint;
use branchyard_sandbox::{ExecSpec, Process, ProviderError};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Identity for the snapshot commits, on both sides.
const IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "Branchyard"),
    ("GIT_AUTHOR_EMAIL", "branchyard@localhost"),
    ("GIT_COMMITTER_NAME", "Branchyard"),
    ("GIT_COMMITTER_EMAIL", "branchyard@localhost"),
];

/// Variables that would point git at another repository.
const SCRUBBED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
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
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    for var in SCRUBBED_ENV {
        command.env_remove(var);
    }
    command.envs(IDENTITY).envs(env.iter().cloned());
    let out = command
        .output()
        .map_err(|e| Error::Host(format!("could not run git: {e}")))?;
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
    endpoint: &Endpoint,
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
pub fn push(endpoint: &Endpoint, worktree: &Path, guest_dir: &Path) -> Result<Pushed, Error> {
    push_staged(endpoint, worktree, guest_dir, None)
}

/// [`push`], staging in `stage` (replaced if it exists), so that whoever
/// recovers from a crash knows what to delete.
pub fn push_staged(
    endpoint: &Endpoint,
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

/// Bring the actor's worktree back and apply it to the host `worktree`,
/// which must still hold exactly the files [`push`] sent.
pub fn pull(endpoint: &Endpoint, pushed: &Pushed, worktree: &Path) -> Result<Pulled, Error> {
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

    let env = stage.env("pull.index");
    host(worktree, &["read-tree", &pushed.snapshot], &env)?;
    let (clean, _, stderr) = host_output(worktree, &["update-index", "--refresh"], &env)?;
    if !clean {
        return Err(Error::WorktreeChanged(stderr.trim().to_owned()));
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
    Ok(Pulled { result, changed })
}

/// Copy the host directory `from` to `to` in the actor, if it exists.
pub fn push_tree(endpoint: &Endpoint, from: &Path, to: &Path) -> Result<(), Error> {
    if !from.is_dir() {
        return Ok(());
    }
    Ok(endpoint.put_tree(from, to)?)
}

/// Replace the host directory `to` with the actor's `from`, if the actor
/// has it. The new tree is received beside `to` and swapped in, so a failed
/// transfer leaves `to` as it was.
pub fn pull_tree(endpoint: &Endpoint, from: &Path, to: &Path) -> Result<(), Error> {
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
