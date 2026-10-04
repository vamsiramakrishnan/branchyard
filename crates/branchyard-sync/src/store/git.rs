//! `git+https://…`, `git+ssh://…`, `git+file://…`: a git remote as an
//! object store, through the git CLI and git's own credentials.
//!
//! Each object is a ref, `refs/by-sync/<key>`, pointing at a commit whose
//! tree holds the object as one blob named `data`. A write pushes with
//! `--force-with-lease=<ref>:<expected>` (an empty expectation means "only
//! if absent"); a delete pushes `:<ref>` under the same lease. The
//! generation is the commit's ID. Reads fetch the one ref into a private
//! bare cache. `list` is `git ls-remote`, which reports no sizes, so sizes
//! read as 0 here (quotas do not apply to this backend). Uploads are one
//! push each; there are no partial pushes to resume. Key characters git
//! refuses in ref names (none of the sync layout's) are escaped as `=XX`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Error, Result};
use crate::store::{check_key, check_prefix, Entry, Generation, Object, ObjectStore};

const NAMESPACE: &str = "refs/by-sync/";

pub struct GitStore {
    remote: String,
    cache: PathBuf,
}

fn encode(key: &str) -> String {
    let mut out = String::new();
    for c in key.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '/' | '-' | '_' => out.push(c),
            _ => out.push_str(&format!("={:02X}", c as u32)),
        }
    }
    out
}

fn decode(name: &str) -> String {
    let mut out = String::new();
    let mut chars = name.chars();
    while let Some(c) = chars.next() {
        if c == '=' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(b) = u8::from_str_radix(&hex, 16) {
                out.push(b as char);
                continue;
            }
            out.push('=');
            out.push_str(&hex);
        } else {
            out.push(c);
        }
    }
    out
}

impl GitStore {
    /// The remote at `url` (without `git+`), cached under `cache`, else
    /// under `~/.cache/branchyard/sync-git/<hash of the URL>`.
    pub fn open(url: &str, cache: Option<&Path>) -> Result<GitStore> {
        let cache = match cache {
            Some(c) => c.to_path_buf(),
            None => {
                let base = std::env::var_os("XDG_CACHE_HOME")
                    .map(PathBuf::from)
                    .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
                    .unwrap_or_else(std::env::temp_dir);
                base.join("branchyard/sync-git")
                    .join(hex::encode(&blake3::hash(url.as_bytes()).as_bytes()[..12]))
            }
        };
        if !cache.join("HEAD").exists() {
            std::fs::create_dir_all(&cache)?;
            run(&cache, &["init", "--bare", "--quiet"], None)?;
        }
        Ok(GitStore {
            remote: url.to_owned(),
            cache,
        })
    }

    fn write_commit(&self, key: &str, data: &[u8]) -> Result<String> {
        let blob = run(&self.cache, &["hash-object", "-w", "--stdin"], Some(data))?;
        let tree = run(
            &self.cache,
            &["mktree"],
            Some(format!("100644 blob {}\tdata\n", blob.trim()).as_bytes()),
        )?;
        // A nonce in the message makes every write a new commit, so a
        // write of the same bytes is still a change git's lease checks.
        let message = format!("{key}\n\n{}", hex::encode(&crate::util::random_bytes(8)?));
        let commit = run(
            &self.cache,
            &["commit-tree", tree.trim(), "-m", &message],
            None,
        )?;
        Ok(commit.trim().to_owned())
    }

    fn push(&self, refspec: &str, lease: &str) -> Result<()> {
        let expected = lease.rsplit(':').next().unwrap_or("");
        if !expected.is_empty() && !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::precondition(format!(
                "{expected:?} is not a commit ID"
            )));
        }
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.cache)
            .args(["push", "--quiet", "--porcelain"])
            .arg(format!("--force-with-lease={lease}"))
            .arg(&self.remote)
            .arg(refspec)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .output()?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if output.status.success() {
            return Ok(());
        }
        if text.contains("stale info") || text.contains("rejected") || text.contains("fetch first")
        {
            return Err(Error::precondition(format!(
                "{refspec}: the remote ref moved"
            )));
        }
        Err(Error::transient(format!("git push: {}", text.trim())))
    }

    fn fetch(&self, key: &str) -> Result<String> {
        let refname = format!("{NAMESPACE}{}", encode(key));
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.cache)
            .args(["fetch", "--quiet", "--no-tags", "--no-write-fetch-head"])
            .arg(&self.remote)
            .arg(format!("+{refname}:refs/cache/{}", encode(key)))
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            let text = String::from_utf8_lossy(&output.stderr);
            if text.contains("couldn't find remote ref") {
                return Err(Error::not_found(format!("{key} is not in the remote")));
            }
            return Err(Error::transient(format!("git fetch: {}", text.trim())));
        }
        Ok(run(
            &self.cache,
            &["rev-parse", &format!("refs/cache/{}", encode(key))],
            None,
        )?
        .trim()
        .to_owned())
    }

    fn remote_refs(&self) -> Result<Vec<(String, String)>> {
        let output = Command::new("git")
            .args(["ls-remote", "--refs"])
            .arg(&self.remote)
            .arg(format!("{NAMESPACE}*"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            return Err(Error::transient(format!(
                "git ls-remote: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let (oid, name) = line.split_once('\t')?;
                let key = name.strip_prefix(NAMESPACE)?;
                Some((decode(key), oid.to_owned()))
            })
            .collect())
    }
}

fn run(dir: &Path, args: &[&str], input: Option<&[u8]>) -> Result<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "branchyard-sync")
        .env("GIT_AUTHOR_EMAIL", "sync@branchyard.invalid")
        .env("GIT_COMMITTER_NAME", "branchyard-sync")
        .env("GIT_COMMITTER_EMAIL", "sync@branchyard.invalid")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(input) = input {
        let mut stdin = child.stdin.take().ok_or_else(|| Error::local("no stdin"))?;
        stdin.write_all(input)?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(Error::local(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

impl ObjectStore for GitStore {
    fn url(&self) -> String {
        format!("git+{}", self.remote)
    }

    fn get(&self, key: &str) -> Result<Object> {
        check_key(key)?;
        let commit = self.fetch(key)?;
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.cache)
            .args(["cat-file", "blob", &format!("{commit}:data")])
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            return Err(Error::corrupt(format!("{key}: the commit holds no data")));
        }
        Ok(Object {
            data: output.stdout,
            generation: commit,
        })
    }

    fn get_range(&self, key: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        let data = self.get(key)?.data;
        let s = (start as usize).min(data.len());
        let e = s.saturating_add(len as usize).min(data.len());
        Ok(data[s..e].to_vec())
    }

    fn stat(&self, key: &str) -> Result<Option<Entry>> {
        check_key(key)?;
        Ok(self
            .remote_refs()?
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(k, oid)| Entry {
                key: k,
                size: 0,
                generation: oid,
                modified_ms: None,
            }))
    }

    fn put_if_absent(&self, key: &str, data: &[u8]) -> Result<Generation> {
        check_key(key)?;
        let commit = self.write_commit(key, data)?;
        let refname = format!("{NAMESPACE}{}", encode(key));
        self.push(&format!("{commit}:{refname}"), &format!("{refname}:"))?;
        Ok(commit)
    }

    fn put_if_match(&self, key: &str, data: &[u8], generation: &str) -> Result<Generation> {
        check_key(key)?;
        let commit = self.write_commit(key, data)?;
        let refname = format!("{NAMESPACE}{}", encode(key));
        self.push(
            &format!("{commit}:{refname}"),
            &format!("{refname}:{generation}"),
        )?;
        Ok(commit)
    }

    fn list(&self, prefix: &str) -> Result<Vec<Entry>> {
        check_prefix(prefix)?;
        let mut out: Vec<Entry> = self
            .remote_refs()?
            .into_iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(key, oid)| Entry {
                key,
                size: 0,
                generation: oid,
                modified_ms: None,
            })
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    fn delete_if_match(&self, key: &str, generation: &str) -> Result<()> {
        check_key(key)?;
        let refname = format!("{NAMESPACE}{}", encode(key));
        match self.push(&format!(":{refname}"), &format!("{refname}:{generation}")) {
            // Deleting a ref that is not there is refused as a failed
            // lease too.
            Err(e) if e.is(crate::Kind::Transient) && e.message.contains("unable to delete") => {
                Err(Error::precondition(format!("{key} does not exist")))
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_through_ref_names() {
        for key in ["tasks/a.b/manifest", "chunks/ab/cd~e", "x"] {
            assert_eq!(decode(&encode(key)), key);
        }
        assert_eq!(encode("a.b"), "a=2Eb");
    }
}
