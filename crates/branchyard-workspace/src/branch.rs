//! Validated names for Branchyard-owned branches.

use std::fmt;
use std::str::FromStr;

/// Prefix of every branch Branchyard creates, so its branches never collide
/// with the user's and can be enumerated.
pub const BRANCH_PREFIX: &str = "by/";

const MAX_LEN: usize = 100;

/// The `<name>` in `by/<name>`.
///
/// One or more `/`-separated segments, each starting with `[a-z0-9]` and
/// continuing with `[a-z0-9._-]`, without `..`, a trailing `.`, or a `.lock`
/// suffix; at most 100 bytes. This is deliberately stricter than
/// `git check-ref-format --branch`, so every accepted name also passes it and
/// is safe to place in an argument vector (it never starts with `-`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BranchName(String);

/// A rejected [`BranchName`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidBranchName(pub String);

impl fmt::Display for InvalidBranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} is not a valid Branchyard branch name (lowercase segments of [a-z0-9._-] separated by '/')",
            self.0
        )
    }
}

impl std::error::Error for InvalidBranchName {}

impl BranchName {
    pub fn new(name: impl Into<String>) -> Result<Self, InvalidBranchName> {
        let name = name.into();
        if valid(&name) {
            Ok(Self(name))
        } else {
            Err(InvalidBranchName(name))
        }
    }

    /// Parses a full branch name (`by/<name>`) or ref (`refs/heads/by/<name>`).
    pub fn from_branch(branch: &str) -> Option<Self> {
        let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
        Self::new(short.strip_prefix(BRANCH_PREFIX)?).ok()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `by/<name>`.
    pub fn branch(&self) -> String {
        format!("{BRANCH_PREFIX}{}", self.0)
    }

    /// `refs/heads/by/<name>`.
    pub fn ref_name(&self) -> String {
        format!("refs/heads/{BRANCH_PREFIX}{}", self.0)
    }
}

fn valid(name: &str) -> bool {
    name.len() <= MAX_LEN
        && !name.contains("..")
        && name.split('/').all(|segment| {
            let mut bytes = segment.bytes();
            matches!(bytes.next(), Some(b'a'..=b'z' | b'0'..=b'9'))
                && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
                && !segment.ends_with('.')
                && !segment.ends_with(".lock")
        })
}

impl fmt::Display for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for BranchName {
    type Err = InvalidBranchName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl AsRef<str> for BranchName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
