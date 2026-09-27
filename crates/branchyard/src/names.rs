//! Branch names: a slug of the prompt, made unique with `-2`, `-3`, ...
//!
//! A name is free when it has no record or reservation in `.branchyard/`
//! and no `by/<name>` git branch, which a merged branch keeps after its
//! record is removed.

use std::path::Path;

use branchyard_workspace::BranchName;

use crate::git;
use crate::state::Store;
use crate::Error;

const SLUG_MAX: usize = 40;
/// Suffixes tried before giving up on an automatic name.
const ATTEMPTS: usize = 1000;

/// Lowercase `[a-z0-9-]`, words joined by single hyphens, at most 40
/// characters, cut at a word boundary where there is one.
pub(crate) fn slug(prompt: &str) -> String {
    let mut slug = String::new();
    for c in prompt.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let mut slug = slug.trim_end_matches('-').to_owned();
    if slug.len() > SLUG_MAX {
        let cut = match slug[..=SLUG_MAX].rfind('-') {
            Some(boundary) if boundary > 0 => boundary,
            _ => SLUG_MAX,
        };
        slug.truncate(cut);
        slug = slug.trim_end_matches('-').to_owned();
    }
    if slug.is_empty() {
        "task".into()
    } else {
        slug
    }
}

/// Check that `name` is a usable single-segment branch name.
pub(crate) fn validate(name: &str) -> Result<BranchName, Error> {
    let invalid = |reason: String| Error::InvalidName {
        name: name.to_owned(),
        reason,
    };
    if name.contains('/') {
        return Err(invalid(
            "local mode uses single-segment names, without '/'".into(),
        ));
    }
    BranchName::new(name).map_err(|e| invalid(e.to_string()))
}

/// Names for one task: `[base]` with no harnesses, else `<base>-<harness>`
/// for each.
fn candidates(base: &str, harnesses: &[&str]) -> Vec<String> {
    if harnesses.is_empty() {
        vec![base.to_owned()]
    } else {
        harnesses.iter().map(|h| format!("{base}-{h}")).collect()
    }
}

fn free(store: &Store, root: &Path, name: &str) -> Result<bool, Error> {
    Ok(!store.taken(name) && !git::branch_exists(root, &format!("by/{name}"))?)
}

/// The names [`reserve`] would take now, without taking them.
pub(crate) fn plan(
    store: &Store,
    root: &Path,
    explicit: Option<&str>,
    prompt: &str,
    harnesses: &[&str],
) -> Result<Vec<String>, Error> {
    search(store, root, explicit, prompt, harnesses, |names| {
        for name in names {
            if !free(store, root, name)? {
                return Ok(false);
            }
        }
        Ok(true)
    })
}

/// The one name [`reserve`] would take now for a single branch, skipping
/// the names in `exclude` (others planned alongside it).
pub(crate) fn plan_one(
    store: &Store,
    root: &Path,
    explicit: Option<&str>,
    prompt: &str,
    exclude: &std::collections::BTreeSet<String>,
) -> Result<String, Error> {
    let mut names = search(store, root, explicit, prompt, &[], |names| {
        for name in names {
            if exclude.contains(name) || !free(store, root, name)? {
                return Ok(false);
            }
        }
        Ok(true)
    })?;
    Ok(names.remove(0))
}

/// Reserve every name for one task, all or none.
pub(crate) fn reserve(
    store: &Store,
    root: &Path,
    explicit: Option<&str>,
    prompt: &str,
    harnesses: &[&str],
) -> Result<Vec<String>, Error> {
    search(store, root, explicit, prompt, harnesses, |names| {
        let mut taken = Vec::new();
        for name in names {
            if free(store, root, name)? && store.reserve(name)? {
                taken.push(name);
            } else {
                for name in taken {
                    store.release(name);
                }
                return Ok(false);
            }
        }
        Ok(true)
    })
}

/// Try `take` on the explicit name's set, or on the slug's with increasing
/// suffixes, until it succeeds.
fn search(
    store: &Store,
    root: &Path,
    explicit: Option<&str>,
    prompt: &str,
    harnesses: &[&str],
    mut take: impl FnMut(&[String]) -> Result<bool, Error>,
) -> Result<Vec<String>, Error> {
    let mut seen = std::collections::BTreeSet::new();
    for harness in harnesses {
        if !seen.insert(*harness) {
            return Err(Error::State(format!("harness {harness} is listed twice")));
        }
    }
    if let Some(name) = explicit {
        let names = candidates(name, harnesses);
        for name in &names {
            validate(name)?;
        }
        return match take(&names)? {
            true => Ok(names),
            false => {
                let taken = names
                    .iter()
                    .find(|n| !matches!(free(store, root, n), Ok(true)))
                    .unwrap_or(&names[0]);
                Err(Error::BranchExists(taken.clone()))
            }
        };
    }
    let base = slug(prompt);
    for attempt in 1..=ATTEMPTS {
        let stem = match attempt {
            1 => base.clone(),
            n => format!("{base}-{n}"),
        };
        let names = candidates(&stem, harnesses);
        for name in &names {
            validate(name)?;
        }
        if take(&names)? {
            return Ok(names);
        }
    }
    Err(Error::State(format!(
        "no free name for {base} after {ATTEMPTS} attempts"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_lowercase_words_cut_at_a_boundary() {
        assert_eq!(
            slug("Make the flaky parser test deterministic"),
            "make-the-flaky-parser-test-deterministic"
        );
        assert_eq!(slug("  Fix: `foo()` -- now!  "), "fix-foo-now");
        assert_eq!(slug("Ünïcode ok"), "n-code-ok");
        assert_eq!(slug("!!!"), "task");
        let long = slug("one two three four five six seven eight nine ten");
        assert_eq!(long, "one-two-three-four-five-six-seven-eight");
        assert!(long.len() <= SLUG_MAX);
        assert_eq!(slug(&"x".repeat(60)), "x".repeat(40));
        assert_eq!(
            slug("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa bbbb"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[test]
    fn names_are_single_valid_segments() {
        assert!(validate("fix-parser-2").is_ok());
        assert!(matches!(validate("a/b"), Err(Error::InvalidName { .. })));
        assert!(matches!(validate("Fix"), Err(Error::InvalidName { .. })));
        assert!(matches!(validate("-x"), Err(Error::InvalidName { .. })));
        assert_eq!(candidates("t", &["codex", "goose"]), ["t-codex", "t-goose"]);
        assert_eq!(candidates("t", &[]), ["t"]);
    }
}
