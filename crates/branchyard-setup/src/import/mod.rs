//! Workspace configuration another worktree tool already committed to the
//! repository, read so `by init project` can suggest the same `[workspace]`:
//! emdash's `.emdash.json` ([`emdash`], ported from emdash), Orca's
//! `orca.yaml` ([`orca`], ported from Orca), Superset's
//! `.superset/config.json` ([`superset`]) and Conductor's
//! `.conductor/settings.toml` ([`conductor`]), the last two written from
//! their public documentation only.
//!
//! Each importer turns its file into the parts `[workspace]` has: files to
//! copy, setup commands, a run script and teardown commands, with the
//! tool's own variables (`$EMDASH_PORT`, `$CONDUCTOR_ROOT_PATH`, ...)
//! renamed to Branchyard's (`$BRANCHYARD_PORT`, `$BRANCHYARD_ROOT`, ...),
//! and a note for each thing it left out. Nothing runs: the result is a
//! suggestion the person reviews, and `[workspace]` scripts run only once
//! trusted (`by workspace trust`).

pub mod conductor;
pub mod emdash;
pub mod orca;
pub mod superset;

use crate::probe::Probe;

/// What one tool's file says about a new worktree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Imported {
    /// The file it came from, relative to the repository root.
    pub file: &'static str,
    /// Globs of untracked files to copy into each worktree.
    pub copy: Vec<String>,
    /// Setup commands, in order.
    pub setup: Vec<String>,
    /// The command that starts a development server.
    pub run: Option<String>,
    pub teardown: Vec<String>,
    /// What was not imported, and why.
    pub notes: Vec<String>,
}

impl Imported {
    pub fn is_empty(&self) -> bool {
        self.copy.is_empty()
            && self.setup.is_empty()
            && self.run.is_none()
            && self.teardown.is_empty()
    }
}

/// A file's text and whether a path (relative to the root) exists.
pub type Reader<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Each importer: its file, and how it reads it.
type Importer = fn(&str, Reader) -> Result<Imported, String>;

/// The files, in the order their suggestions take precedence.
pub const IMPORTERS: &[(&str, Importer)] = &[
    (emdash::FILE, emdash::import),
    (orca::FILE, orca::import),
    (superset::FILE, superset::import),
    (conductor::FILE, conductor::import),
];

/// Every importable file in the repository, read; a file that cannot be
/// read is a note, not an import.
pub fn detect(probe: &dyn Probe) -> (Vec<Imported>, Vec<String>) {
    let read = |path: &str| probe.read(path);
    let mut found = Vec::new();
    let mut notes = Vec::new();
    for (file, import) in IMPORTERS {
        let Some(text) = probe.read(file) else {
            continue;
        };
        match import(&text, &read) {
            Ok(imported) => {
                notes.extend(imported.notes.iter().map(|n| format!("{file}: {n}")));
                found.push(imported);
            }
            Err(error) => notes.push(format!("{file} was not imported: {error}")),
        }
    }
    (found, notes)
}

/// `command` with each `$NAME` and `${NAME}` of `names` renamed to its
/// Branchyard variable.
pub fn rename_variables(command: &str, names: &[(&str, &str)]) -> String {
    let mut out = command.to_owned();
    for (from, to) in names {
        out = out.replace(&format!("${{{from}}}"), &format!("${{{to}}}"));
        let mut result = String::new();
        let mut rest = out.as_str();
        let needle = format!("${from}");
        while let Some(at) = rest.find(&needle) {
            let after = &rest[at + needle.len()..];
            result.push_str(&rest[..at]);
            if after.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
                result.push_str(&needle);
            } else {
                result.push('$');
                result.push_str(to);
            }
            rest = after;
        }
        result.push_str(rest);
        out = result;
    }
    out
}

/// A multi-line script as setup commands: one per line when every line
/// stands alone, else the whole script as one command, so shell structure
/// (`if`, loops, continuations) is never cut apart. Blank lines and
/// comments are dropped.
pub fn script_commands(script: &str) -> Vec<String> {
    let lines: Vec<&str> = script
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    const OPENERS: &[&str] = &[
        "if ", "for ", "while ", "until ", "case ", "fi", "done", "esac", "}", "else", "elif ",
        "then", "do", ")",
    ];
    const CONTINUES: &[&str] = &["\\", "|", "&&", "||", "then", "do", "{", "(", "in"];
    let structured = lines.iter().any(|line| {
        OPENERS.iter().any(|o| line.starts_with(o)) || CONTINUES.iter().any(|c| line.ends_with(c))
    }) || script.contains("<<");
    match (structured, lines.len()) {
        (_, 0) => Vec::new(),
        (true, _) => vec![script.trim().to_owned()],
        (false, _) => lines.into_iter().map(str::to_owned).collect(),
    }
}

/// `command` run in `cwd`, a directory relative to the worktree.
pub fn in_directory(cwd: Option<&str>, command: String) -> String {
    match cwd.map(str::trim).filter(|c| !c.is_empty() && *c != ".") {
        Some(cwd) => format!("cd {} && {command}", quote_word(cwd)),
        None => command,
    }
}

/// `word` quoted for `sh` where it needs it.
pub fn quote_word(word: &str) -> String {
    // Only a NUL byte cannot be quoted, and no shell word holds one.
    shlex::try_quote(&word.replace('\0', ""))
        .map(|quoted| quoted.into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_are_renamed_only_where_whole() {
        let names = [
            ("TOOL_PORT", "BRANCHYARD_PORT"),
            ("TOOL_ROOT", "BRANCHYARD_ROOT"),
        ];
        assert_eq!(
            rename_variables(
                "PORT=$TOOL_PORT cp \"${TOOL_ROOT}/.env\" . $TOOL_PORTS $TOOL_ROOT",
                &names
            ),
            "PORT=$BRANCHYARD_PORT cp \"${BRANCHYARD_ROOT}/.env\" . $TOOL_PORTS $BRANCHYARD_ROOT"
        );
    }

    #[test]
    fn scripts_split_into_lines_only_when_each_stands_alone() {
        assert_eq!(
            script_commands("node setup.mjs\n\n# deps\npnpm install\n"),
            ["node setup.mjs", "pnpm install"]
        );
        assert_eq!(
            script_commands("if [ -f x ]; then\n  make\nfi\n"),
            ["if [ -f x ]; then\n  make\nfi"]
        );
        assert_eq!(script_commands("a \\\n  b"), ["a \\\n  b"]);
        assert!(script_commands("\n# only a comment\n").is_empty());
        assert_eq!(
            in_directory(Some("apps/web"), "bun dev".into()),
            "cd apps/web && bun dev"
        );
        assert_eq!(in_directory(Some("."), "bun dev".into()), "bun dev");
        assert_eq!(in_directory(Some("my app"), "x".into()), "cd 'my app' && x");
        for word in ["it's", "a b", "", "$x `y`", "plain-1.2/x"] {
            assert_eq!(shlex::split(&quote_word(word)).unwrap(), [word], "{word:?}");
        }
        assert_eq!(quote_word("plain"), "plain");
    }
}
