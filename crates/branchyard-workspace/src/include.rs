// Derived from Orca (https://github.com/stablyai/orca) at
// 280733273545f0b3eeedc1be54b14d406239030e: src/main/git/worktree-include-file.ts.
// Copyright (c) 2026 Lovecast Inc. Licensed under the MIT License; see
// vendor/orca/LICENSE.
//
// Modified for Branchyard: translated from TypeScript to Rust. Parsing and
// the limits (256 KiB, 1000 entries) follow Orca; entries are checked in
// one `git check-ignore` call instead of a concurrent stat fan-out; a
// skipped entry is returned with its reason for the caller to record
// instead of logged; `.branchyard` is refused as `.git` is, and a `.`
// or empty segment is refused rather than normalized.

//! `.worktreeinclude`: the cross-tool file at a repository's root that
//! names git-ignored files and directories to carry into each new worktree
//! (a `.env`, `.vscode/`). Only literal paths are honoured; glob (`*`,
//! `?`) and negation (`!`) lines are skipped and named. Only entries that
//! exist in the primary checkout **and** that git ignores are returned:
//! tracked files are already in a fresh worktree, and copying untracked
//! files that are not ignored would make spurious changes.

use std::fs;
use std::path::Path;

use crate::git::Git;

/// The file's name, at the repository root.
pub const WORKTREE_INCLUDE_FILE: &str = ".worktreeinclude";
/// A larger file is ignored whole.
pub const MAX_FILE_BYTES: u64 = 256 * 1024;
/// Entries beyond this many are ignored.
pub const MAX_ENTRIES: usize = 1000;

/// What `.worktreeinclude` names, resolved against a checkout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Included {
    /// Repository-relative paths to copy, sorted: each exists in the
    /// checkout and is ignored by git.
    pub paths: Vec<String>,
    /// Entries not honoured, each with why (`entry: reason`). An entry
    /// absent from the checkout is not listed: it has nothing to copy.
    pub skipped: Vec<String>,
}

/// The file's entries: blank lines and `#` comments skipped, `\`
/// normalized to `/`, a leading `./` and trailing `/` stripped, duplicates
/// dropped, in order. Each is anchored at the repository root.
pub fn parse(content: &str) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut entries = Vec::new();
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let slashed = line.replace('\\', "/");
        let normalized = slashed
            .strip_prefix("./")
            .unwrap_or(&slashed)
            .trim_end_matches('/')
            .to_owned();
        if normalized.is_empty() || !seen.insert(normalized.clone()) {
            continue;
        }
        entries.push(normalized);
    }
    entries
}

fn unsupported(entry: &str) -> bool {
    entry.starts_with('!') || entry.contains('*') || entry.contains('?')
}

/// Why `entry` may not be copied, if it may not: it must be relative, stay
/// inside the repository, and not reach into `.git` or `.branchyard`.
pub fn unsafe_reason(entry: &str) -> Option<&'static str> {
    if entry.is_empty() || entry.starts_with('/') || Path::new(entry).is_absolute() {
        return Some("must be relative to the repository root");
    }
    let segments: Vec<&str> = entry.split('/').collect();
    if segments
        .iter()
        .any(|s| *s == ".." || s.is_empty() || *s == ".")
    {
        return Some("must be a plain path inside the repository");
    }
    if segments[0] == ".git" || segments[0] == ".branchyard" {
        return Some("must not reach into .git or .branchyard");
    }
    None
}

/// Resolve `root`'s `.worktreeinclude` against the checkout at `root`.
/// Never fails: a missing, oversized or unreadable file, or a git that
/// cannot be run, resolves to nothing (the latter two named in
/// `skipped`), so creating a worktree is never blocked by this file.
pub fn resolve(root: &Path) -> Included {
    let mut included = Included::default();
    let path = root.join(WORKTREE_INCLUDE_FILE);
    let Ok(meta) = fs::symlink_metadata(&path) else {
        return included;
    };
    if !meta.is_file() {
        return included;
    }
    if meta.len() > MAX_FILE_BYTES {
        included.skipped.push(format!(
            "{WORKTREE_INCLUDE_FILE}: larger than {MAX_FILE_BYTES} bytes; ignored"
        ));
        return included;
    }
    let Ok(content) = fs::read_to_string(&path) else {
        included
            .skipped
            .push(format!("{WORKTREE_INCLUDE_FILE}: could not be read"));
        return included;
    };
    let mut candidates = Vec::new();
    for entry in parse(&content) {
        if candidates.len() >= MAX_ENTRIES {
            included.skipped.push(format!(
                "{WORKTREE_INCLUDE_FILE}: more than {MAX_ENTRIES} entries; the rest are ignored"
            ));
            break;
        }
        if unsupported(&entry) {
            included.skipped.push(format!(
                "{entry}: only literal files and directories are supported, not globs or negation"
            ));
            continue;
        }
        if let Some(why) = unsafe_reason(&entry) {
            included.skipped.push(format!("{entry}: {why}"));
            continue;
        }
        // Absent in the checkout (node_modules before an install): nothing
        // to copy.
        if fs::symlink_metadata(root.join(&entry)).is_ok() {
            candidates.push(entry);
        }
    }
    if candidates.is_empty() {
        return included;
    }
    match ignored(root, &candidates) {
        Ok(ignored) => {
            for entry in candidates {
                match ignored.contains(&entry) {
                    true => included.paths.push(entry),
                    false => included.skipped.push(format!(
                        "{entry}: git does not ignore it (tracked or plain untracked files are \
                         not carried over)"
                    )),
                }
            }
            included.paths.sort();
        }
        Err(why) => included
            .skipped
            .push(format!("{WORKTREE_INCLUDE_FILE}: {why}")),
    }
    included
}

/// Which of `paths` git ignores at `root`: one `git check-ignore`. Tracked
/// paths are never reported, whatever the ignore rules say.
fn ignored(root: &Path, paths: &[String]) -> Result<std::collections::BTreeSet<String>, String> {
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    let (output, _) = Git::new(root)
        .args(["check-ignore", "-z", "--stdin"])
        .stdin(input)
        .output()
        .map_err(|e| format!("could not check which entries git ignores: {e}"))?;
    // 1: none of them is ignored.
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(format!(
            "git check-ignore failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).trim_end_matches('/').to_owned())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_normalized_deduplicated_and_comments_skipped() {
        let text = "# secrets\n.env\n./.env\n\n  .vscode/  \nconfig\\local.json\n.env\n";
        assert_eq!(parse(text), [".env", ".vscode", "config/local.json"]);
    }

    #[test]
    fn unsafe_and_pattern_entries_are_refused() {
        for bad in [
            "/etc/passwd",
            "../x",
            "a/../b",
            ".git/config",
            ".branchyard/x",
            "a//b",
        ] {
            assert!(unsafe_reason(bad).is_some(), "{bad}");
        }
        assert!(unsafe_reason("config/local.json").is_none());
        assert!(unsupported("*.env") && unsupported("!keep") && unsupported("a?"));
    }
}
