//! Ownership scopes: the repository paths a delegated child may change.
//!
//! A parent splits work by ownership. It gives each child the paths it
//! owns (`by spawn --owns`, a plan's `owns`), and the child's changes
//! anywhere else are refused when it is integrated (`outside`), before
//! anything moves. Siblings that own disjoint paths then cannot conflict,
//! and no child reaches past its assignment.
//!
//! A scope is set by the parent, in the child's grant, never by the child.
//! Each branch keeps its own entries, and a path is in a branch's scope
//! only when every scope from the branch up to the root covers it
//! (`chain`). A child given none is bounded by its parent's, and a scope
//! only narrows from parent to child, exactly. A spawn whose entries cannot
//! overlap its parent's scope at all is refused (`check_overlap`). The
//! child is told its scope in its instructions every turn
//! (`assignment`).
//!
//! An entry is a repository-relative path or a glob ([`normalize`]):
//!
//! - a path without glob characters (`src/parser`, `docs/api.md`) is that
//!   file, or that directory and everything below it;
//! - a glob (`src/**/*.rs`, `tests/parser_*.rs`) matches with `*` and `?`
//!   inside one path component and `**` across components.
//!
//! What counts is what the child would bring into its parent. A path the
//! child changed whose content is already its parent's is not its change:
//! a sibling's work it merged to reconcile with, once that sibling landed,
//! or that lands before it in the same integration.
//!
//! Not guaranteed: a scope bounds what is integrated, not what a harness
//! writes in its own worktree, where a shell can write anywhere. The
//! refusal comes at integration, and `by check` reports it before then.

use std::collections::BTreeSet;

use crate::state::{Record, Store};
use crate::{git, Error, Yard};

/// Characters that make an entry a glob.
const META: &[char] = &['*', '?', '['];

/// Paths a refusal names before it says how many more there are.
const NAMED: usize = 8;

fn is_glob(entry: &str) -> bool {
    entry.contains(META)
}

/// One entry as it is stored: trimmed and relative to the repository, with
/// a leading `./` and trailing `/` removed. Refused when it is empty,
/// absolute, leaves the repository, or is not a usable glob.
pub fn normalize(entry: &str) -> Result<String, Error> {
    let mut text = entry.trim();
    while let Some(rest) = text.strip_prefix("./") {
        text = rest;
    }
    let text = text.trim_end_matches('/');
    let refuse = |why: &str| {
        Err(Error::Denied(format!(
            "owned path {entry:?} {why}; name a file, a directory or a glob relative to \
             the repository, such as src/parser or tests/parser_*.rs"
        )))
    };
    if text.is_empty() || text == "." {
        return refuse("is empty");
    }
    if text.starts_with('/') {
        return refuse("is absolute");
    }
    if text.contains('\\') {
        return refuse("uses \\; separate components with /");
    }
    if text
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return refuse("must stay inside the repository, with no empty, . or .. component");
    }
    if is_glob(text) {
        if let Err(error) = glob::Pattern::new(text) {
            return Err(Error::Denied(format!(
                "owned path {entry:?} is not a usable glob: {error}"
            )));
        }
    }
    Ok(text.to_owned())
}

/// Every entry [`normalize`]d, without duplicates, in the order given.
pub fn normalize_all(entries: &[String]) -> Result<Vec<String>, Error> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for entry in entries {
        let entry = normalize(entry)?;
        if seen.insert(entry.clone()) {
            out.push(entry);
        }
    }
    Ok(out)
}

fn options() -> glob::MatchOptions {
    glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    }
}

/// Whether `entry` covers repository path `path`.
fn entry_covers(entry: &str, path: &str) -> bool {
    match is_glob(entry) {
        false => {
            path == entry
                || path
                    .strip_prefix(entry)
                    .is_some_and(|rest| rest.starts_with('/'))
        }
        true => glob::Pattern::new(entry).is_ok_and(|p| p.matches_with(path, options())),
    }
}

/// Whether one scope covers `path`. An empty scope covers every path.
pub fn covers(scope: &[String], path: &str) -> bool {
    scope.is_empty() || scope.iter().any(|entry| entry_covers(entry, path))
}

/// Whether every scope in `chain` covers `path`: whether `path` is in the
/// scope of the branch the chain is for.
pub fn in_scope(chain: &[Vec<String>], path: &str) -> bool {
    chain.iter().all(|scope| covers(scope, path))
}

/// The directory every path `entry` matches lies under: the entry itself
/// for a path, and a glob's components before the first with a glob
/// character. Empty for a glob that starts with one.
fn literal_dir(entry: &str) -> String {
    match is_glob(entry) {
        false => entry.to_owned(),
        true => entry
            .split('/')
            .take_while(|c| !c.contains(META))
            .collect::<Vec<_>>()
            .join("/"),
    }
}

/// Whether `outer` is `inner` or one of its ancestors, as directories.
fn is_dir_prefix(outer: &str, inner: &str) -> bool {
    outer.is_empty() || inner == outer || inner.starts_with(&format!("{outer}/"))
}

/// Whether some path could be covered by both `entry` and `scope`: false
/// only when their directories are disjoint, so no path lies under both.
pub fn may_overlap(entry: &str, scope: &[String]) -> bool {
    if scope.is_empty() {
        return true;
    }
    let mine = literal_dir(entry);
    scope.iter().any(|other| {
        let theirs = literal_dir(other);
        is_dir_prefix(&mine, &theirs) || is_dir_prefix(&theirs, &mine)
    })
}

/// The scopes bounding `record`, its own first, then each ancestor's that
/// has one: a path is in its scope when all of them cover it. Empty when
/// none has a scope, and every path is in scope.
pub(crate) fn chain(store: &Store, record: &Record) -> Result<Vec<Vec<String>>, Error> {
    let mut chain = Vec::new();
    let mut seen = BTreeSet::new();
    let mut current = Some(record.clone());
    while let Some(branch) = current {
        if !seen.insert(branch.info.name.clone()) {
            break;
        }
        if let Some(owns) = branch
            .grant
            .as_ref()
            .map(|g| &g.owns)
            .filter(|o| !o.is_empty())
        {
            chain.push(owns.clone());
        }
        current = match &branch.info.parent {
            Some(parent) => Some(store.read(parent)?),
            None => None,
        };
    }
    Ok(chain)
}

/// Refuse a child's `owns` when an entry could not overlap the scope its
/// parent works under (`parent_chain`): no path it names could ever be
/// integrated.
pub(crate) fn check_overlap(
    parent: &str,
    owns: &[String],
    parent_chain: &[Vec<String>],
) -> Result<(), Error> {
    for entry in owns {
        if let Some(scope) = parent_chain.iter().find(|s| !may_overlap(entry, s)) {
            return Err(Error::Denied(format!(
                "a child of {parent} may not own {entry}: it lies outside {parent}'s scope \
                 ({}), so none of its changes could be integrated; give it paths inside that \
                 scope",
                scope.join(", ")
            )));
        }
    }
    Ok(())
}

/// The paths of `head` that a branch bounded by `chain` and started from
/// `base` would bring into `target`, but its scope does not cover. A path
/// whose content in `head` is already `target`'s, or that of a candidate in
/// `earlier` (merged before it in the same integration), is not its
/// change.
pub(crate) fn outside(
    yard: &Yard,
    chain: &[Vec<String>],
    base: &str,
    head: &str,
    target: &str,
    earlier: &[String],
) -> Result<Vec<String>, Error> {
    if chain.is_empty() || base.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<String> = git::changed_paths(yard.root(), base, head)?
        .into_iter()
        .filter(|path| !in_scope(chain, path))
        .collect();
    for other in std::iter::once(target).chain(earlier.iter().map(String::as_str)) {
        if out.is_empty() {
            break;
        }
        let differs: BTreeSet<String> = git::changed_paths(yard.root(), other, head)?
            .into_iter()
            .collect();
        out.retain(|path| differs.contains(path));
    }
    Ok(out)
}

/// `paths`, the first [`NAMED`] of them, then how many more.
pub(crate) fn named(paths: &[String]) -> String {
    let shown: Vec<&str> = paths.iter().take(NAMED).map(String::as_str).collect();
    match paths.len().saturating_sub(NAMED) {
        0 => shown.join(", "),
        more => format!("{} and {more} more", shown.join(", ")),
    }
}

/// `chain` as one line: the branch's own entries, then each ancestor's
/// scope it works within.
pub(crate) fn describe(chain: &[Vec<String>]) -> String {
    let mut parts = chain.iter().map(|scope| scope.join(", "));
    match parts.next() {
        None => "the whole repository".into(),
        Some(own) => {
            let within: Vec<String> = parts.collect();
            match within.is_empty() {
                true => own,
                false => format!("{own}, within {}", within.join("; within ")),
            }
        }
    }
}

/// Why `branch`'s integration was refused: `paths` it changed outside the
/// scope `chain` bounds it by.
pub(crate) fn refusal(branch: &str, chain: &[Vec<String>], paths: &[String]) -> Error {
    Error::Denied(format!(
        "{branch} changed {} outside the paths it owns ({}): {}; nothing was integrated. \
         Send it a turn to undo or move those changes (`by send {branch} \"<prompt>\"`), or \
         make them yourself",
        match paths.len() {
            1 => "1 file".to_owned(),
            n => format!("{n} files"),
        },
        describe(chain),
        named(paths)
    ))
}

/// What a branch bounded by `chain` is told in its instructions every
/// turn; `None` without a scope.
pub(crate) fn assignment(chain: &[Vec<String>]) -> Option<String> {
    if chain.is_empty() {
        return None;
    }
    let list = |scope: &[String]| {
        scope
            .iter()
            .map(|e| format!("`{e}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut text = format!(
        "## The paths you own\n\
         Your parent gave you ownership of {}.",
        list(&chain[0])
    );
    for scope in &chain[1..] {
        text.push_str(&format!(" You work within {}, too.", list(scope)));
    }
    text.push_str(
        " Change files only there: when your parent integrates your work, a change \
         anywhere else is refused and nothing of yours lands. If the task needs a change \
         outside them, do not make it; say what and why in your final message, and your \
         parent will make it or give it to whoever owns it.",
    );
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|e| (*e).to_owned()).collect()
    }

    #[test]
    fn entries_are_normalized_and_bad_ones_refused() {
        assert_eq!(normalize("./src/parser/").unwrap(), "src/parser");
        assert_eq!(normalize("  tests/*.rs ").unwrap(), "tests/*.rs");
        for bad in [
            "", ".", "/etc", "../x", "src/../x", "src//x", "a\\b", "src/[x",
        ] {
            assert!(normalize(bad).is_err(), "{bad:?} should be refused");
        }
        assert_eq!(
            normalize_all(&owned(&["src", "./src/", "docs"])).unwrap(),
            owned(&["src", "docs"])
        );
    }

    #[test]
    fn a_path_covers_its_tree_and_a_glob_its_matches() {
        let scope = owned(&["src/parser", "tests/parser_*.rs", "docs/**/*.md"]);
        for path in [
            "src/parser",
            "src/parser/lex.rs",
            "src/parser/deep/x.rs",
            "tests/parser_lex.rs",
            "docs/a.md",
            "docs/api/b.md",
        ] {
            assert!(covers(&scope, path), "{path} should be covered");
        }
        for path in [
            "src/parsers.rs",
            "src/main.rs",
            "tests/parser/x.rs",
            "docs/a.txt",
        ] {
            assert!(!covers(&scope, path), "{path} should not be covered");
        }
        assert!(covers(&[], "anything/at/all"));
    }

    #[test]
    fn a_chain_covers_a_path_only_when_every_scope_does() {
        let chain = vec![owned(&["src/**/*.rs"]), owned(&["src/parser"])];
        assert!(in_scope(&chain, "src/parser/lex.rs"));
        assert!(!in_scope(&chain, "src/main.rs"), "outside the parent's");
        assert!(!in_scope(&chain, "src/parser/README.md"), "outside its own");
        assert!(in_scope(&[], "x"));
    }

    #[test]
    fn disjoint_entries_cannot_overlap() {
        let parent = owned(&["src/parser"]);
        assert!(may_overlap("src/parser/lex.rs", &parent));
        assert!(
            may_overlap("src", &parent),
            "an ancestor directory overlaps"
        );
        assert!(
            may_overlap("**/*.rs", &parent),
            "a glob from the root may match"
        );
        assert!(may_overlap("src/*/x.rs", &parent));
        assert!(!may_overlap("docs", &parent));
        assert!(!may_overlap("src/printer/**", &parent));
        assert!(check_overlap(
            "lead",
            &owned(&["src/parser/a"]),
            std::slice::from_ref(&parent)
        )
        .is_ok());
        let refused = check_overlap("lead", &owned(&["docs"]), &[parent]).unwrap_err();
        assert!(
            refused.to_string().contains("outside lead's scope"),
            "{refused}"
        );
    }

    #[test]
    fn the_assignment_and_refusal_name_the_scope() {
        assert_eq!(assignment(&[]), None);
        let chain = vec![owned(&["src/parser"]), owned(&["src"])];
        let text = assignment(&chain).unwrap();
        assert!(text.contains("`src/parser`"), "{text}");
        assert!(text.contains("within `src`"), "{text}");
        let paths: Vec<String> = (0..10).map(|i| format!("f{i}")).collect();
        let error = refusal("child", &chain, &paths).to_string();
        assert!(error.contains("10 files"), "{error}");
        assert!(error.contains("src/parser, within src"), "{error}");
        assert!(error.contains("and 2 more"), "{error}");
    }
}
