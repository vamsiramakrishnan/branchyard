//! `by review <branch>`: the branch's diff in your editor, where you write
//! comments under the lines they are about; on save, the comments become
//! one prompt (formatted as Orca formats review notes, [`review_format`])
//! sent to the branch as a single `by send`. `--print` shows the prompt
//! instead.
//!
//! The review file is the diff with comment lines added. A comment line
//! starts with `>>`; more `>>` lines continue it. Its anchor is the diff
//! line above it: a context or added line is that line of the branch's
//! file, a removed line the line above where it was removed, a hunk header
//! (`@@`) the hunk's lines, and a file header (`diff --git`, `---`, `+++`,
//! `index`, ...) the whole file. Lines starting with `#` are instructions
//! and ignored. The diff itself must stay as it was, so a stray edit cannot
//! move a comment to the wrong line: [`parse`] refuses one, naming it.
//!
//! [`review_format`]: crate::review_format

use std::path::{Path, PathBuf};

use crate::args::{Permissions, TaskArgs};
use crate::commands::{self, print, Env, Failure, Outcome, Target};
use crate::open;
use crate::review_format::{format_diff_comments, DiffComment};

/// What starts a comment line.
pub const MARKER: &str = ">>";

/// The review file's instructions, above the diff.
fn header(branch: &str) -> String {
    format!(
        "# by review {branch}: comment on this branch's changes, then save and quit.\n\
         # Write a comment on its own line starting with \">>\" under the line it is\n\
         # about; more \">>\" lines continue it. Under a file's header it is about the\n\
         # whole file, under an @@ line about that hunk's lines. Leave the diff as it\n\
         # is; lines starting with \"#\" are ignored. All comments go to {branch} as one\n\
         # prompt; with none, nothing is sent.\n"
    )
}

/// Where a parse failed and why.
#[derive(Debug, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based, in the review file.
    pub line: usize,
    pub message: String,
}

/// What a comment written at a point of the diff is about.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Anchor {
    Nothing,
    File(String),
    Lines(String, Option<u32>, u32),
}

/// Walks a unified diff, tracking the file and the branch side's line.
struct Walker {
    file: Option<String>,
    /// The branch-side line the next context or added line has.
    next: u32,
    in_hunk: bool,
}

fn header_path(rest: &str, prefix: &str) -> Option<String> {
    let path = rest.split('\t').next()?.trim_end();
    if path == "/dev/null" {
        return None;
    }
    Some(path.strip_prefix(prefix).unwrap_or(path).to_owned())
}

impl Walker {
    /// The anchor after `line`, a line of the diff.
    fn step(&mut self, line: &str) -> Anchor {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // `a/old b/new`; the new path unless a `+++` line corrects it.
            self.file = rest
                .rsplit_once(" b/")
                .map(|(_, path)| path.to_owned())
                .or_else(|| Some(rest.to_owned()));
            self.in_hunk = false;
            return self.file_anchor();
        }
        if !self.in_hunk {
            if let Some(rest) = line.strip_prefix("+++ ") {
                if let Some(path) = header_path(rest, "b/") {
                    self.file = Some(path);
                }
                return self.file_anchor();
            }
            if let Some(rest) = line.strip_prefix("--- ") {
                if self.file.is_none() {
                    self.file = header_path(rest, "a/");
                }
                return self.file_anchor();
            }
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            // `-a,b +c,d @@ ...`
            let new = rest
                .split_whitespace()
                .find_map(|part| part.strip_prefix('+'))
                .unwrap_or("1");
            let (start, count) = match new.split_once(',') {
                Some((start, count)) => (start.parse().unwrap_or(1), count.parse().unwrap_or(1)),
                None => (new.parse().unwrap_or(1), 1),
            };
            self.next = start;
            self.in_hunk = true;
            let Some(file) = self.file.clone() else {
                return Anchor::Nothing;
            };
            return match count {
                0 => Anchor::Lines(file, None, start.max(1)),
                1 => Anchor::Lines(file, None, start),
                n => Anchor::Lines(file, Some(start), start + n - 1),
            };
        }
        if !self.in_hunk {
            return self.file_anchor();
        }
        let Some(file) = self.file.clone() else {
            return Anchor::Nothing;
        };
        match line.as_bytes().first() {
            Some(b'-') => Anchor::Lines(file, None, self.next.saturating_sub(1).max(1)),
            Some(b'\\') => Anchor::Lines(file, None, self.next.saturating_sub(1).max(1)),
            // A context line (editors may strip its lone space) or an added one.
            _ => {
                let here = self.next;
                self.next += 1;
                Anchor::Lines(file, None, here)
            }
        }
    }

    fn file_anchor(&self) -> Anchor {
        match &self.file {
            Some(file) => Anchor::File(file.clone()),
            None => Anchor::Nothing,
        }
    }
}

/// The review file for `diff` on `branch`.
pub fn template(branch: &str, diff: &str) -> String {
    let mut text = header(branch);
    text.push_str(diff);
    if !diff.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// The comments in `edited`, the review file for `diff` after editing, in
/// the order written; or where the diff itself was changed.
#[allow(clippy::expect_used)] // ratchet: branchyard-cli
pub fn parse(diff: &str, edited: &str) -> Result<Vec<DiffComment>, ParseError> {
    let original: Vec<&str> = diff.lines().collect();
    let mut walker = Walker {
        file: None,
        next: 1,
        in_hunk: false,
    };
    let mut at = 0;
    let mut anchor = Anchor::Nothing;
    let mut comments: Vec<DiffComment> = Vec::new();
    // Whether the previous line was a comment, which this one continues.
    let mut continuing = false;
    for (index, line) in edited.lines().enumerate() {
        let number = index + 1;
        if let Some(text) = line.strip_prefix(MARKER) {
            let text = text.strip_prefix(' ').unwrap_or(text).trim_end();
            if continuing {
                let last = comments.last_mut().expect("continuing a comment");
                last.body.push('\n');
                last.body.push_str(text);
                continue;
            }
            let (file_path, start_line, line_number) = match &anchor {
                Anchor::Nothing => {
                    return Err(ParseError {
                        line: number,
                        message: "a comment must come after the diff line it is about".into(),
                    })
                }
                Anchor::File(file) => (file.clone(), None, 0),
                Anchor::Lines(file, start, line) => (file.clone(), *start, *line),
            };
            comments.push(DiffComment {
                file_path,
                start_line,
                line_number,
                body: text.to_owned(),
            });
            continuing = true;
            continue;
        }
        continuing = false;
        if line.starts_with('#') {
            continue;
        }
        match original.get(at) {
            Some(expected) if expected.trim_end() == line.trim_end() => {
                anchor = walker.step(expected);
                at += 1;
            }
            Some(expected) => {
                return Err(ParseError {
                    line: number,
                    message: format!(
                        "the diff was edited here: expected {expected:?}, found {line:?}; write \
                         comments on lines starting with {MARKER} and leave the diff as it is"
                    ),
                })
            }
            None if line.trim().is_empty() => {}
            None => {
                return Err(ParseError {
                    line: number,
                    message: format!(
                        "{line:?} follows the end of the diff; start a comment with {MARKER}"
                    ),
                })
            }
        }
    }
    if at < original.len() {
        return Err(ParseError {
            line: edited.lines().count(),
            message: format!(
                "the diff ends early: {} of its lines are missing (from {:?})",
                original.len() - at,
                original[at]
            ),
        });
    }
    for comment in &mut comments {
        comment.body = comment.body.trim().to_owned();
    }
    comments.retain(|c| !c.body.is_empty());
    Ok(comments)
}

/// The prompt for `comments`, deterministic for the same comments.
pub fn message(comments: &[DiffComment]) -> String {
    let files: std::collections::BTreeSet<&str> =
        comments.iter().map(|c| c.file_path.as_str()).collect();
    format!(
        "Review comments on this branch's changes ({} comment{} on {} file{}), written on its \
         diff with by review. A line number is the line in the file as this branch has it.\n\n\
         {}\n\nAddress each comment on this branch.\n",
        comments.len(),
        if comments.len() == 1 { "" } else { "s" },
        files.len(),
        if files.len() == 1 { "" } else { "s" },
        format_diff_comments(comments)
    )
}

/// What `by review` was asked to do.
pub struct ReviewArgs<'a> {
    pub branch: &'a str,
    pub print_only: bool,
    pub editor: Option<&'a str>,
    /// An edited review file to read instead of opening an editor.
    pub file: Option<&'a str>,
    pub detach: bool,
    pub task: &'a TaskArgs,
}

/// Where the review file of `branch` is kept until its comments are sent,
/// so a cancelled or failed review is not lost.
fn draft_path(target: &Target, branch: &str) -> Result<PathBuf, Failure> {
    let safe: String = branch
        .chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                true => c,
                false => '_',
            },
        )
        .collect();
    let dir = match target {
        Target::Local => commands::open()?.root().join(".branchyard").join("review"),
        Target::Remote(_) => std::env::temp_dir().join(format!(
            "branchyard-review-{}",
            std::env::var("USER").unwrap_or_default()
        )),
    };
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{safe}.diff")))
}

/// `by review`.
pub fn main(env: &Env, target: &Target, args: &ReviewArgs) -> Outcome {
    if std::env::var_os(branchyard::ENV_BRANCH).is_some_and(|v| !v.is_empty()) {
        return Err(Failure::Sdk(branchyard::Error::Denied(
            "by review is for a person reading a branch's diff in an editor; inside a harness, \
             send the branch a prompt with by send"
                .into(),
        )));
    }
    if args.detach {
        let carried = TaskArgs {
            permissions: args.task.permissions,
            command: args.task.command.clone(),
            ..TaskArgs::default()
        };
        if carried != *args.task {
            return Err(Failure::Message(
                "--detach carries only --yes, --ask and --command to the background by send; \
                 drop the other options or leave out --detach"
                    .into(),
            ));
        }
    }
    let branch = args.branch;
    let diff = match target {
        Target::Local => commands::open()?.branch(branch)?.diff()?,
        Target::Remote(remote) => remote.repo.diff(branch)?,
    };
    if diff.trim().is_empty() {
        return Err(Failure::Message(format!(
            "{branch} has no changes to review"
        )));
    }
    let (path, edited) = match args.file {
        Some(file) => {
            let text = std::fs::read_to_string(file)
                .map_err(|e| Failure::Message(format!("--file {file}: {e}")))?;
            (PathBuf::from(file), text)
        }
        None => {
            let path = draft_path(target, branch)?;
            // A draft left by an earlier review of the same diff keeps its
            // comments; one of another diff is replaced.
            let reuse = std::fs::read_to_string(&path)
                .ok()
                .filter(|draft| parse(&diff, draft).is_ok());
            if reuse.is_none() {
                std::fs::write(&path, template(branch, &diff))?;
            } else {
                eprintln!("by: reopening your unsent review of {branch}");
            }
            let plan = open::plan(&path, args.editor, &|name| std::env::var(name).ok())?;
            eprintln!("by: reviewing {branch} in {}", plan.argv[0]);
            open::launch(&plan)?;
            let text = std::fs::read_to_string(&path)?;
            (path, text)
        }
    };
    let comments = parse(&diff, &edited).map_err(|e| {
        Failure::Message(format!(
            "{}:{}: {}; nothing was sent. Fix it and run by review {branch} again (the file \
             is kept), or pass it with --file",
            path.display(),
            e.line,
            e.message
        ))
    })?;
    if comments.is_empty() {
        if args.file.is_none() {
            branchyard_support::cleanup_file(&path);
        }
        eprintln!("by: no comments, so nothing was sent to {branch}");
        return Ok(());
    }
    let text = message(&comments);
    if args.print_only {
        if args.file.is_none() {
            eprintln!(
                "by: not sent; your comments are kept in {} for the next by review {branch}",
                path.display()
            );
        }
        return print(&text);
    }
    eprintln!(
        "by: sending {} comment{} to {branch}",
        comments.len(),
        if comments.len() == 1 { "" } else { "s" }
    );
    if args.detach {
        detach(target, branch, &text, args.task, &path)?;
    } else {
        commands::send(
            env,
            target,
            branch,
            commands::Prompt::Text(&text),
            args.task,
            false,
            false,
        )?;
    }
    if args.file.is_none() {
        branchyard_support::cleanup_file(&path);
    }
    Ok(())
}

/// Start `by send` for the review in the background, in its own process
/// group, with its output in a log beside the draft, and return.
fn detach(target: &Target, branch: &str, text: &str, task: &TaskArgs, draft: &Path) -> Outcome {
    let by = std::env::current_exe()?;
    let mut command = std::process::Command::new(by);
    if let Target::Remote(remote) = target {
        command.args(&remote.args);
    }
    command.arg("send");
    match task.permissions {
        Permissions::Yes => {
            command.arg("--yes");
        }
        Permissions::Ask => {
            command.arg("--ask");
        }
        Permissions::Preset(preset) => {
            command.args(["--permissions", preset.name()]);
        }
        Permissions::Unset => {}
    }
    if let Some(argv) = &task.command {
        let line: Vec<String> = argv.iter().map(|a| crate::args::shell_quote(a)).collect();
        command.arg("--command").arg(line.join(" "));
    }
    command.args(["--", branch, text]);
    let log = draft.with_extension(format!("{}.log", branchyard_support::time::now_ms()));
    let file = std::fs::File::create(&log)?;
    command
        .stdin(std::process::Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file);
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn()?;
    eprintln!(
        "by: sending in the background; its output goes to {}",
        log.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,4 +1,5 @@
 fn a() {}
-fn b() {}
+fn b() -> u32 { 1 }
+fn c() {}

 fn d() {}
diff --git a/old.txt b/old.txt
deleted file mode 100644
index 3333333..0000000
--- a/old.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-one
-two
";

    fn comment(file: &str, start: Option<u32>, line: u32, body: &str) -> DiffComment {
        DiffComment {
            file_path: file.into(),
            start_line: start,
            line_number: line,
            body: body.into(),
        }
    }

    /// Insert `>>` lines after the diff lines matching each needle.
    fn edit(after: &[(&str, &str)]) -> String {
        let mut out = String::new();
        let mut pending: Vec<(&str, &str)> = after.to_vec();
        for line in template("feat", DIFF).lines() {
            out.push_str(line);
            out.push('\n');
            pending.retain(|(needle, text)| {
                if line == *needle {
                    out.push_str(text);
                    out.push('\n');
                    return false;
                }
                true
            });
        }
        assert!(pending.is_empty(), "{pending:?} not found");
        out
    }

    #[test]
    fn comments_anchor_to_the_line_above_them() {
        let edited = edit(&[
            ("+++ b/src/lib.rs", ">> Split this file"),
            ("@@ -1,4 +1,5 @@", ">> This hunk needs a test"),
            ("+fn b() -> u32 { 1 }", ">> Why 1?\n>>   and not 0\n>>"),
            ("-fn b() {}", ">> b was public API"),
            (" fn d() {}", ">>d"),
            ("-two", ">> Keep this file"),
        ]);
        let comments = parse(DIFF, &edited).unwrap();
        assert_eq!(
            comments,
            [
                comment("src/lib.rs", None, 0, "Split this file"),
                comment("src/lib.rs", Some(1), 5, "This hunk needs a test"),
                comment("src/lib.rs", None, 1, "b was public API"),
                comment("src/lib.rs", None, 2, "Why 1?\n  and not 0"),
                comment("src/lib.rs", None, 5, "d"),
                comment("old.txt", None, 1, "Keep this file"),
            ]
        );
        let text = message(&comments);
        assert!(text.starts_with(
            "Review comments on this branch's changes (6 comments on 2 files), written on its \
             diff with by review."
        ));
        assert!(text.contains(
            "File: src/lib.rs\nLine: 2\nUser comment: \"Why 1?\\n  and not 0\"\n\n\
             File: src/lib.rs\nLine: 5\nUser comment: \"d\""
        ));
        assert!(text.contains("File: src/lib.rs\nLines: 1-5\nUser comment:"));
        assert!(text.contains("File: src/lib.rs\nScope: file\nUser comment:"));
        assert!(text.ends_with("\n\nAddress each comment on this branch.\n"));
        // The same file always gives the same message.
        assert_eq!(message(&parse(DIFF, &edited).unwrap()), text);
    }

    #[test]
    fn an_untouched_file_has_no_comments_and_an_edited_diff_is_refused() {
        assert_eq!(parse(DIFF, &template("feat", DIFF)).unwrap(), []);
        // Editors that strip trailing spaces turn the blank context line
        // into an empty one; that is not an edit.
        let stripped: String = template("feat", DIFF)
            .lines()
            .map(|l| format!("{}\n", l.trim_end()))
            .collect();
        assert_eq!(parse(DIFF, &stripped).unwrap(), []);
        let changed = template("feat", DIFF).replace("+fn c() {}", "+fn c() { todo!() }");
        let error = parse(DIFF, &changed).unwrap_err();
        assert_eq!(error.line, 15);
        assert!(
            error.message.contains("the diff was edited here"),
            "{error:?}"
        );
        let early = format!("{}>> a comment with no line\n{DIFF}", header("feat"));
        assert!(parse(DIFF, &early)
            .unwrap_err()
            .message
            .contains("after the diff line"));
        let cut: String = template("feat", DIFF)
            .lines()
            .take(10)
            .map(|l| format!("{l}\n"))
            .collect();
        assert!(parse(DIFF, &cut)
            .unwrap_err()
            .message
            .contains("ends early"));
        let empty_comment = edit(&[(" fn a() {}", ">>   ")]);
        assert_eq!(parse(DIFF, &empty_comment).unwrap(), []);
    }
}
