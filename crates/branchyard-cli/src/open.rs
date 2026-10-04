//! `by open`: a branch's worktree in the user's editor. [`plan`] and
//! [`launch`] are separate so `by watch` can bind a key to them and leave
//! its screen first for an editor that takes over the terminal.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::args;
use crate::commands::{self, print, Failure, Outcome, Target};

/// Editors known by a short name, with the command each installs.
/// `terminal` editors take over the terminal until they exit.
pub const KNOWN_EDITORS: &[(&str, &str, bool)] = &[
    ("code", "code", false),
    ("vscode", "code", false),
    ("code-insiders", "code-insiders", false),
    ("cursor", "cursor", false),
    ("windsurf", "windsurf", false),
    ("zed", "zed", false),
    ("subl", "subl", false),
    ("sublime", "subl", false),
    ("idea", "idea", false),
    ("intellij", "idea", false),
    ("goland", "goland", false),
    ("pycharm", "pycharm", false),
    ("webstorm", "webstorm", false),
    ("rustrover", "rustrover", false),
    ("fleet", "fleet", false),
    ("xcode", "xed", false),
    ("vim", "vim", true),
    ("nvim", "nvim", true),
    ("neovim", "nvim", true),
    ("vi", "vi", true),
    ("emacs", "emacs", true),
    ("hx", "hx", true),
    ("helix", "hx", true),
    ("nano", "nano", true),
    ("micro", "micro", true),
    ("kak", "kak", true),
];

/// How an editor will be started on a worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The command, then its arguments, then the worktree.
    pub argv: Vec<String>,
    /// Where the editor came from: `--editor`, `$VISUAL` or `$EDITOR`.
    pub source: &'static str,
    /// It runs in this terminal until it exits, so a full-screen caller
    /// must leave its screen first.
    pub terminal: bool,
}

/// The editor for `worktree`: `editor` (a known name or a command line),
/// else `$VISUAL`, else `$EDITOR`. `env` reads a variable.
pub fn plan(
    worktree: &Path,
    editor: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Plan, Failure> {
    let set = |name: &str| env(name).filter(|v| !v.trim().is_empty());
    let (line, source) = match editor {
        Some(editor) => (editor.to_owned(), "--editor"),
        None => match (set("VISUAL"), set("EDITOR")) {
            (Some(visual), _) => (visual, "$VISUAL"),
            (None, Some(editor)) => (editor, "$EDITOR"),
            (None, None) => {
                return Err(Failure::Message(format!(
                    "no editor: set $VISUAL or $EDITOR, or pass --editor NAME ({}); \
                     `by open --print` prints the worktree's path",
                    names()
                )))
            }
        },
    };
    let mut argv =
        args::split_words(&line).map_err(|e| Failure::Message(format!("{source}: {e}")))?;
    if argv.is_empty() {
        return Err(Failure::Message(format!("{source} names no command")));
    }
    let known = KNOWN_EDITORS.iter().find(|(name, _, _)| *name == argv[0]);
    if let Some((_, command, _)) = known {
        argv[0] = (*command).to_owned();
    }
    let program = Path::new(&argv[0])
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let terminal = KNOWN_EDITORS
        .iter()
        .find(|(_, command, _)| *command == program)
        .is_some_and(|(_, _, terminal)| *terminal);
    argv.push(worktree.display().to_string());
    Ok(Plan {
        argv,
        source,
        terminal,
    })
}

fn names() -> String {
    let mut seen: Vec<&str> = Vec::new();
    for (name, command, _) in KNOWN_EDITORS {
        if name == command && !seen.contains(name) {
            seen.push(name);
        }
    }
    seen.join(", ")
}

/// Start `plan`'s editor and wait for its command to return (a GUI
/// editor's returns at once). A missing command is refused by name.
pub fn launch(plan: &Plan) -> Outcome {
    let Some((program, rest)) = plan.argv.split_first() else {
        return Err(Failure::Message("the editor plan has no command".into()));
    };
    let status = match Command::new(program).args(rest).status() {
        Ok(status) => status,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Failure::Message(format!(
                "{program} (from {}) is not on PATH; pass --editor with one that is ({})",
                plan.source,
                names()
            )))
        }
        Err(error) => {
            return Err(Failure::Message(format!(
                "could not start {program}: {error}"
            )))
        }
    };
    match status.success() {
        true => Ok(()),
        false => Err(Failure::Message(format!("{program} exited with {status}"))),
    }
}

/// The worktree of `branch` in the local yard, refused when it has none.
pub fn worktree(branch: &str) -> Result<PathBuf, Failure> {
    let info = commands::open()?.branch(branch)?.info().clone();
    if !info.worktree.is_dir() {
        return Err(Failure::Message(format!(
            "{branch} has no worktree at {} (it may be waiting, or removed)",
            info.worktree.display()
        )));
    }
    Ok(info.worktree)
}

pub fn main(target: &Target, branch: &str, editor: Option<&str>, print_only: bool) -> Outcome {
    if let Target::Remote(_) = target {
        return Err(Failure::Sdk(branchyard::Error::Unsupported(
            "by open works in local mode only: a server's worktrees are on the server; \
             run by open there"
                .into(),
        )));
    }
    let worktree = worktree(branch)?;
    if print_only {
        return print(&format!("{}\n", worktree.display()));
    }
    let plan = plan(&worktree, editor, &|name| std::env::var(name).ok())?;
    eprintln!("by: opening {} with {}", worktree.display(), plan.argv[0]);
    launch(&plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn editors_come_from_the_flag_then_visual_then_editor() {
        let tree = Path::new("/w/t");
        let plan1 = plan(tree, Some("cursor"), &env(&[("VISUAL", "code")])).unwrap();
        assert_eq!(plan1.argv, ["cursor", "/w/t"]);
        assert_eq!(plan1.source, "--editor");
        let plan2 = plan(tree, None, &env(&[("VISUAL", "code -n"), ("EDITOR", "vi")])).unwrap();
        assert_eq!(plan2.argv, ["code", "-n", "/w/t"]);
        assert!(!plan2.terminal);
        let plan3 = plan(tree, None, &env(&[("VISUAL", " "), ("EDITOR", "nvim")])).unwrap();
        assert_eq!((plan3.argv[0].as_str(), plan3.source), ("nvim", "$EDITOR"));
        assert!(plan3.terminal);
        let alias = plan(tree, Some("sublime"), &env(&[])).unwrap();
        assert_eq!(alias.argv, ["subl", "/w/t"]);
        let error = plan(tree, None, &env(&[])).unwrap_err().to_string();
        assert!(
            error.contains("no editor") && error.contains("cursor"),
            "{error}"
        );
    }
}
