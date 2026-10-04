//! The git operations sync needs, through the git CLI on one git
//! directory: refs, ancestry, packs of what a remote lacks
//! (`git pack-objects --revs` with the remote's tips excluded), and
//! importing a pack (`git index-pack --stdin --fix-thin`, which checks
//! every object as it indexes it).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::error::{Error, Result};

#[derive(Clone, Debug)]
pub struct Git {
    dir: PathBuf,
}

impl Git {
    pub fn new(git_dir: &Path) -> Git {
        Git {
            dir: git_dir.to_path_buf(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn command(&self) -> Command {
        let mut c = Command::new("git");
        c.arg("--git-dir").arg(&self.dir);
        c.env("GIT_TERMINAL_PROMPT", "0");
        c
    }

    fn output(&self, args: &[&str], input: Option<&[u8]>) -> Result<Output> {
        let mut command = self.command();
        command
            .args(args)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|e| Error::local(format!("git: {e}")))?;
        if let Some(input) = input {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| Error::local("git: no stdin"))?;
            let data = input.to_vec();
            // Write from a thread, so a large output cannot deadlock.
            let writer = std::thread::spawn(move || stdin.write_all(&data));
            let output = child
                .wait_with_output()
                .map_err(|e| Error::local(format!("git: {e}")))?;
            branchyard_support::join_reporting("writer", writer);
            return Ok(output);
        }
        child
            .wait_with_output()
            .map_err(|e| Error::local(format!("git: {e}")))
    }

    fn run(&self, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
        let output = self.output(args, input)?;
        if !output.status.success() {
            return Err(Error::local(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(output.stdout)
    }

    /// Every ref under `prefix` (such as `refs/heads/`), to its object.
    pub fn refs(&self, prefix: &str) -> Result<BTreeMap<String, String>> {
        let out = self.run(
            &["for-each-ref", "--format=%(refname) %(objectname)", prefix],
            None,
        )?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|l| {
                let (name, oid) = l.split_once(' ')?;
                Some((name.to_owned(), oid.to_owned()))
            })
            .collect())
    }

    pub fn resolve(&self, name: &str) -> Result<Option<String>> {
        let output = self.output(&["rev-parse", "--verify", "--quiet", name], None)?;
        Ok(output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned()))
    }

    /// Whether the object is in this repository.
    pub fn has(&self, oid: &str) -> Result<bool> {
        Ok(self
            .output(&["cat-file", "-e", oid], None)?
            .status
            .success())
    }

    /// Whether `a` is `b` or an ancestor of it.
    pub fn is_ancestor(&self, a: &str, b: &str) -> Result<bool> {
        if a == b {
            return Ok(true);
        }
        let output = self.output(&["merge-base", "--is-ancestor", a, b], None)?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(Error::local(format!(
                "git merge-base: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))),
        }
    }

    /// Set `name` to `new`, only if it is at `old` (`None`: absent).
    pub fn update_ref(&self, name: &str, new: &str, old: Option<&str>) -> Result<()> {
        let zero = "0".repeat(new.len());
        self.run(&["update-ref", name, new, old.unwrap_or(&zero)], None)
            .map(|_| ())
    }

    pub fn delete_ref(&self, name: &str, old: &str) -> Result<()> {
        self.run(&["update-ref", "-d", name, old], None).map(|_| ())
    }

    /// Refs checked out in a worktree of this repository, with the
    /// worktree's path.
    pub fn checked_out(&self) -> Result<BTreeMap<String, PathBuf>> {
        let out = match self.output(&["worktree", "list", "--porcelain"], None) {
            Ok(o) if o.status.success() => o.stdout,
            _ => return Ok(BTreeMap::new()),
        };
        let mut found = BTreeMap::new();
        let mut path = None;
        for line in String::from_utf8_lossy(&out).lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(branch) = line.strip_prefix("branch ") {
                if let Some(p) = path.clone() {
                    found.insert(branch.to_owned(), p);
                }
            }
        }
        Ok(found)
    }

    /// Fast-forward the branch checked out in `worktree` to `commit`, when
    /// the worktree has no changes; false when it has some (or the move is
    /// not a fast-forward), leaving it as it was.
    pub fn fast_forward_worktree(&self, worktree: &Path, commit: &str) -> Result<bool> {
        let status = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["status", "--porcelain", "--untracked-files=no"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|e| Error::local(format!("git: {e}")))?;
        if !status.status.success() || !status.stdout.is_empty() {
            return Ok(false);
        }
        let merged = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["merge", "--ff-only", "--quiet", commit])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|e| Error::local(format!("git: {e}")))?;
        Ok(merged.status.success())
    }

    /// A pack of every object reachable from `tips` and not from
    /// `exclude` (tips this repository lacks are skipped), and how many
    /// objects it holds.
    pub fn pack(&self, tips: &[String], exclude: &[String]) -> Result<(Vec<u8>, u64)> {
        let mut input = String::new();
        for tip in tips {
            input.push_str(tip);
            input.push('\n');
        }
        for oid in exclude {
            if self.has(oid)? {
                input.push('^');
                input.push_str(oid);
                input.push('\n');
            }
        }
        let pack = self.run(
            &[
                "pack-objects",
                "--revs",
                "--stdout",
                "-q",
                "--delta-base-offset",
            ],
            Some(input.as_bytes()),
        )?;
        let objects = pack_objects(&pack)?;
        Ok((pack, objects))
    }

    /// Index `pack` into this repository's objects, checking each object.
    pub fn import_pack(&self, pack: &[u8]) -> Result<()> {
        if pack_objects(pack)? == 0 {
            return Ok(());
        }
        let output = self.output(&["index-pack", "--stdin", "--fix-thin"], Some(pack))?;
        if !output.status.success() {
            return Err(Error::corrupt(format!(
                "git index-pack refused a pack: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }

    /// The paths and blob IDs of a commit's tree.
    pub fn tree(&self, commit: &str) -> Result<Vec<(String, String)>> {
        let out = self.run(&["ls-tree", "-r", "-z", commit], None)?;
        Ok(out
            .split(|b| *b == 0)
            .filter(|e| !e.is_empty())
            .filter_map(|e| {
                let text = String::from_utf8_lossy(e);
                let (meta, path) = text.split_once('\t')?;
                let mut fields = meta.split_whitespace();
                let kind = fields.nth(1)?;
                let oid = fields.next()?;
                (kind == "blob").then(|| (path.to_owned(), oid.to_owned()))
            })
            .collect())
    }

    pub fn blob(&self, oid: &str) -> Result<Vec<u8>> {
        self.run(&["cat-file", "blob", oid], None)
    }
}

/// The object count in a pack's header.
pub fn pack_objects(pack: &[u8]) -> Result<u64> {
    if pack.len() < 12 || &pack[..4] != b"PACK" {
        return Err(Error::corrupt("not a git pack"));
    }
    Ok(u32::from_be_bytes([pack[8], pack[9], pack[10], pack[11]]) as u64)
}
