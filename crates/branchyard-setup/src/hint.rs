//! Why a configuration file does not load, said so a person can fix it:
//! the loader's line and key, without the list of every valid key, and a
//! "did you mean" for a key it does not know. The interview uses it when
//! the file it would update is one `by` itself refuses, instead of
//! quietly starting from nothing.

use crate::config::{self, Defaults};

/// The names an old file used for keys that were renamed, and what they
/// are called now.
const RENAMED: &[(&str, &str)] = &[
    ("agent", "harness"),
    ("budget", "budget_usd"),
    ("cost_limit", "budget_usd"),
    ("turns", "max_turns"),
    ("timeout", "max_minutes"),
    ("minutes", "max_minutes"),
    ("test", "check"),
];

/// A configuration file's text the loader refuses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unreadable {
    /// The line the loader names, when it names one.
    pub line: Option<usize>,
    /// The loader's message without its line and without the list of valid
    /// keys: ``unknown field `agent` ``.
    pub message: String,
    /// For a key the loader does not know: the key it probably means.
    pub hint: Option<String>,
}

impl Unreadable {
    /// The problem as one phrase: ``line 2: unknown field `agent` (did you
    /// mean `harness`, under [defaults]?)``.
    pub fn problem(&self) -> String {
        let mut out = String::new();
        if let Some(line) = self.line {
            out.push_str(&format!("line {line}: "));
        }
        out.push_str(&self.message);
        if let Some(hint) = &self.hint {
            out.push_str(&format!(" (did you mean {hint}?)"));
        }
        out
    }
}

/// `Some` when `text` does not load as a Branchyard configuration file.
pub fn unreadable(text: &str) -> Option<Unreadable> {
    let error = config::parse(text).err()?.to_string();
    let (line, rest) = match error.strip_prefix("line ").and_then(|r| r.split_once(": ")) {
        Some((n, rest)) => (n.parse::<usize>().ok(), rest),
        None => (None, error.as_str()),
    };
    // The loader appends every valid key to an unknown one; a person needs
    // the key, and the hint says what to use instead.
    let (message, expected) = match rest.split_once(", expected ") {
        Some((message, tail)) => (message, names(tail)),
        None => (rest, Vec::new()),
    };
    let hint = message
        .strip_prefix("unknown field `")
        .and_then(|r| r.split_once('`'))
        .and_then(|(unknown, _)| suggest(unknown, &expected));
    Some(Unreadable {
        line,
        message: message.to_owned(),
        hint,
    })
}

/// The backquoted names in ``one of `a`, `b` ``.
fn names(text: &str) -> Vec<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

/// The keys of `[defaults]`, from the schema of the type the loader reads.
fn defaults_keys() -> Vec<String> {
    schemars::schema_for!(Defaults)
        .to_value()
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default()
}

/// What `unknown` probably meant, among the keys the loader `expected`
/// there, and, at the top level, the `[defaults]` keys people often write
/// there. Phrased for ``did you mean {}?``.
fn suggest(unknown: &str, expected: &[String]) -> Option<String> {
    let top_level = expected.iter().any(|k| k == "defaults");
    let under_defaults = match top_level {
        true => defaults_keys(),
        false => Vec::new(),
    };
    let place = |key: &str| match (expected.iter().any(|k| k == key), top_level) {
        (false, true) => format!("`{key}`, under [defaults]"),
        _ => format!("`{key}`"),
    };
    if let Some((_, now)) = RENAMED.iter().find(|(old, _)| *old == unknown) {
        if expected.iter().chain(&under_defaults).any(|k| k == now) {
            return Some(place(now));
        }
    }
    // A [defaults] key written at the top level.
    if under_defaults.iter().any(|k| k == unknown) {
        return Some(place(unknown));
    }
    expected
        .iter()
        .chain(&under_defaults)
        .filter(|key| near(unknown, key))
        .min_by_key(|key| distance(unknown, key))
        .map(|key| place(key))
}

/// Whether two keys are plausibly the same word: equal but for case and
/// `-` or `_`, one the start of the other, or a typo or two apart.
fn near(a: &str, b: &str) -> bool {
    let plain = |s: &str| s.replace(['-', '_'], "").to_ascii_lowercase();
    let (a, b) = (plain(a), plain(b));
    a == b
        || (a.len() >= 4 && b.starts_with(&a))
        || (b.len() >= 4 && a.starts_with(&b))
        || distance(&a, &b) <= 2_usize.min(a.len().min(b.len()) / 3)
}

/// The edit distance between two short words.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (previous + usize::from(ca != *cb))
                .min(row[j] + 1)
                .min(above + 1);
            previous = above;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_readable_file_is_not_a_problem() {
        assert_eq!(
            unreadable("version = 1\n[defaults]\nharness = \"codex\"\n"),
            None
        );
        assert_eq!(unreadable(""), None);
    }

    #[test]
    fn an_old_key_is_named_with_its_line_and_what_replaced_it() {
        let found = unreadable("version = 1\nagent = \"claude\"\nbudget = 5\n").unwrap();
        assert_eq!(found.line, Some(2));
        assert_eq!(found.message, "unknown field `agent`");
        assert_eq!(
            found.problem(),
            "line 2: unknown field `agent` (did you mean `harness`, under [defaults]?)"
        );
        let budget = unreadable("version = 1\nbudget = 5\n").unwrap();
        assert!(
            budget
                .problem()
                .ends_with("(did you mean `budget_usd`, under [defaults]?)"),
            "{}",
            budget.problem()
        );
    }

    #[test]
    fn a_defaults_key_at_the_top_level_and_a_typo_are_placed() {
        let top = unreadable("harness = \"codex\"\n").unwrap();
        assert_eq!(top.hint.as_deref(), Some("`harness`, under [defaults]"));
        let typo = unreadable("[defaults]\nbudget_us = 5\n").unwrap();
        assert_eq!(typo.hint.as_deref(), Some("`budget_usd`"), "{typo:?}");
        let table = unreadable("[workspac]\nsetup = \"x\"\n").unwrap();
        assert_eq!(table.hint.as_deref(), Some("`workspace`"), "{table:?}");
    }

    #[test]
    fn a_problem_without_a_better_name_has_no_hint_and_keeps_its_line() {
        let unknown = unreadable("version = 1\nzzzzzz = 1\n").unwrap();
        assert_eq!(unknown.hint, None);
        assert_eq!(unknown.problem(), "line 2: unknown field `zzzzzz`");
        let broken = unreadable("[defaults\n").unwrap();
        assert_eq!(broken.line, Some(1));
        assert_eq!(broken.hint, None);
        let wrong = unreadable("version = 7\n").unwrap();
        assert_eq!(wrong.line, None);
        assert!(wrong.message.contains("version"), "{wrong:?}");
    }
}
