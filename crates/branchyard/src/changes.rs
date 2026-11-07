//! A branch's candidate diff as data, and the review comments written on it.
//!
//! [`Branch::diff`](crate::Branch::diff) is git's unified diff as one text.
//! A client that shows it file by file (the console's Changes view) needs
//! more: the files with their added and removed line counts, each file's
//! hunks with every line numbered on both sides, a bound on how much it has
//! to hold, and binary files named but not shown. [`Changes::of`] reads
//! that from the same diff, bounded by [`MAX_FILES`], [`MAX_FILE_LINES`],
//! [`MAX_TOTAL_LINES`] and [`MAX_LINE_CHARS`]; what the bound left out is
//! marked, and the totals still count it.
//!
//! The rest is `by review` for a client that is not an editor: a
//! [`ReviewComment`] names a file, a line or range and the side of the diff
//! it is on; [`review_prompt`] writes them as one prompt in the format
//! `by review` sends (`File:`, `Line:` or `Lines:`, `User comment:`), with
//! the quoted lines of the diff under each, so the agent reads the code a
//! comment is about and not only its number. A review is recorded as an
//! [`Activity::Feedback`] whose `reference` is the candidate commit the line
//! numbers refer to; [`Branch::reviews`] reads those back, and marks a
//! comment addressed in the candidate that changed its lines since.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{git, Activity, Branch, Error, FeedbackSource};

/// The most files [`Changes`] lists; the rest are counted in
/// [`Changes::omitted_files`].
pub const MAX_FILES: usize = 300;
/// The most diff lines kept for one file; past it the file is
/// [`ChangedFile::truncated`].
pub const MAX_FILE_LINES: usize = 2_000;
/// The most diff lines kept across all files; files after it list no hunks.
pub const MAX_TOTAL_LINES: usize = 20_000;
/// The most characters kept of one line; a longer one ends in `…`.
pub const MAX_LINE_CHARS: usize = 1_000;
/// The most comments one review carries.
pub const MAX_REVIEW_COMMENTS: usize = 100;
/// The most characters of one comment or note.
pub const MAX_REVIEW_TEXT: usize = 8_000;
/// The most quoted lines the prompt shows under one comment.
const MAX_QUOTED: usize = 20;

/// What happened to a file. Serialized in snake case.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    /// The file is new.
    Added,
    /// The file is gone.
    Deleted,
    /// The file's content (or mode) changed.
    #[default]
    Modified,
    /// The file moved, possibly with edits; see [`ChangedFile::old_path`].
    Renamed,
}

/// What a diff line is. Serialized in snake case.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    /// Unchanged, shown for context: on both sides.
    Context,
    /// Added by the branch: on the new side only.
    Add,
    /// Removed by the branch: on the old side only.
    Del,
    /// git's `\ No newline at end of file`: on neither.
    NoNewline,
}

/// One line of a hunk.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    /// Context, added or removed.
    pub kind: LineKind,
    /// The line's number in the base's file; none for an added line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<u32>,
    /// The line's number in the branch's file; none for a removed line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<u32>,
    /// The line without its `+`, `-` or space marker, cut at
    /// [`MAX_LINE_CHARS`].
    pub text: String,
}

/// One `@@` hunk of a file.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    /// The `@@ -a,b +c,d @@ section` line.
    pub header: String,
    /// The first line of the hunk in the base's file.
    pub old_start: u32,
    /// How many base lines it covers.
    pub old_lines: u32,
    /// The first line of the hunk in the branch's file.
    pub new_start: u32,
    /// How many branch lines it covers.
    pub new_lines: u32,
    /// Its lines, in order; fewer than the hunk holds when the file is
    /// [`ChangedFile::truncated`].
    pub lines: Vec<DiffLine>,
}

/// One file the candidate changes.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    /// The file's path in the branch (in the base, for a deleted file).
    pub path: String,
    /// Where a renamed file was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// Added, deleted, modified or renamed.
    pub status: FileStatus,
    /// Lines added, counted in full even when the hunks are cut.
    pub additions: u32,
    /// Lines removed, counted in full even when the hunks are cut.
    pub deletions: u32,
    /// A binary file: named, with no hunks.
    #[serde(default, skip_serializing_if = "is_false")]
    pub binary: bool,
    /// Some of its lines were left out to keep the answer bounded.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
    /// Its hunks.
    pub hunks: Vec<Hunk>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// A branch's candidate against its base, file by file.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    /// The base the candidate is compared with.
    pub base: String,
    /// The candidate's commit; none while the branch has no candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The changed files, in git's order, at most [`MAX_FILES`].
    pub files: Vec<ChangedFile>,
    /// Lines added across every changed file, including those not listed.
    pub additions: u32,
    /// Lines removed across every changed file, including those not listed.
    pub deletions: u32,
    /// Files beyond [`MAX_FILES`] that are counted in the totals only.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_files: u32,
    /// Whether any file or the whole answer was cut.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// Which side of the diff a comment's lines are on. Serialized in snake
/// case; `new` when absent.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffSide {
    /// The base's file: the lines the branch removed (and context).
    Old,
    /// The branch's file: the lines it added (and context).
    #[default]
    New,
}

/// One comment written on the diff.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewComment {
    /// The file's path in the branch, as [`ChangedFile::path`].
    pub path: String,
    /// The (last) line, numbered on `side`.
    pub line: u32,
    /// The first line of a range, when it covers more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    /// The side the lines are numbered on.
    #[serde(default)]
    pub side: DiffSide,
    /// What the reviewer wrote.
    pub body: String,
}

/// A comment as it was sent, and whether the branch has since changed its
/// lines.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentComment {
    /// The file.
    pub path: String,
    /// The (last) line; 0 for a comment on the whole file (`by review`).
    pub line: u32,
    /// The first line of a range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    /// The side the lines are numbered on.
    #[serde(default)]
    pub side: DiffSide,
    /// What the reviewer wrote.
    pub body: String,
    /// The candidate commit that changed these lines after the comment was
    /// sent; none while they stand as they were (or for an old-side
    /// comment, which is never marked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addressed_in: Option<String>,
}

/// One review that was sent to a branch.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    /// The number, from 1, of its event in the branch's log.
    pub id: u64,
    /// When it was sent, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// The candidate commit its line numbers refer to; none for a review
    /// sent by a client that did not record one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The reviewer's overall note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Its comments, in the order sent.
    pub comments: Vec<SentComment>,
}

/// The reviews sent to a branch, oldest first.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reviews {
    /// The branch's candidate now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    /// Every review.
    pub reviews: Vec<Review>,
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(MAX_LINE_CHARS) {
        None => text.to_owned(),
        Some((end, _)) => format!("{}…", &text[..end]),
    }
}

/// A path as git prints it: unquoted, with C escapes decoded.
fn unquote(raw: &str) -> String {
    let Some(inner) = raw.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return raw.to_owned();
    };
    let bytes = inner.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'\\' || at + 1 >= bytes.len() {
            out.push(bytes[at]);
            at += 1;
            continue;
        }
        at += 1;
        match bytes[at] {
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'0'..=b'7' => {
                let mut value = 0u32;
                let mut digits = 0;
                while digits < 3 && at < bytes.len() && (b'0'..=b'7').contains(&bytes[at]) {
                    value = value * 8 + u32::from(bytes[at] - b'0');
                    at += 1;
                    digits += 1;
                }
                out.push(u8::try_from(value & 0xff).unwrap_or(b'?'));
                continue;
            }
            other => out.push(other),
        }
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The path of a `---` or `+++` line's rest, or none for `/dev/null`.
fn header_path(rest: &str, prefix: &str) -> Option<String> {
    let path = unquote(rest.split('\t').next().unwrap_or(rest).trim_end());
    if path == "/dev/null" {
        return None;
    }
    Some(path.strip_prefix(prefix).map(str::to_owned).unwrap_or(path))
}

/// The old and new path of a `diff --git a/x b/y` line's rest.
fn git_header_paths(rest: &str) -> (String, String) {
    let plain = |s: &str, prefix: &str| {
        let s = unquote(s);
        s.strip_prefix(prefix).map(str::to_owned).unwrap_or(s)
    };
    // The common case: the same path on both sides splits at the middle.
    if !rest.starts_with('"') && rest.len() % 2 == 1 {
        let mid = rest.len() / 2;
        if let (Some(left), Some(right)) = (rest.get(..mid), rest.get(mid + 1..)) {
            if rest.as_bytes()[mid] == b' ' && left.strip_prefix("a/") == right.strip_prefix("b/") {
                return (plain(left, "a/"), plain(right, "b/"));
            }
        }
    }
    match rest.rsplit_once(" b/") {
        Some((old, new)) => (plain(old, "a/"), new.to_owned()),
        None => match rest.rsplit_once(" \"b/") {
            Some((old, new)) => (plain(old, "a/"), plain(&format!("\"b/{new}"), "b/")),
            None => (plain(rest, "a/"), plain(rest, "a/")),
        },
    }
}

/// `-a,b +c,d` of an `@@` line as (old start, old lines, new start, new
/// lines).
fn hunk_ranges(header: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = header.strip_prefix("@@ ")?;
    let mut parts = rest.split_whitespace();
    let range = |part: Option<&str>, sign: char| -> Option<(u32, u32)> {
        let part = part?.strip_prefix(sign)?;
        Some(match part.split_once(',') {
            Some((start, count)) => (start.parse().ok()?, count.parse().ok()?),
            None => (part.parse().ok()?, 1),
        })
    };
    let (old_start, old_lines) = range(parts.next(), '-')?;
    let (new_start, new_lines) = range(parts.next(), '+')?;
    Some((old_start, old_lines, new_start, new_lines))
}

/// Reads a unified diff, one line at a time, into bounded [`Changes`].
struct Parser {
    changes: Changes,
    file: Option<ChangedFile>,
    hunk: Option<Hunk>,
    old_left: u32,
    new_left: u32,
    old_no: u32,
    new_no: u32,
    kept_total: usize,
    kept_file: usize,
}

impl Parser {
    fn new(changes: Changes) -> Self {
        Parser {
            changes,
            file: None,
            hunk: None,
            old_left: 0,
            new_left: 0,
            old_no: 0,
            new_no: 0,
            kept_total: 0,
            kept_file: 0,
        }
    }

    fn flush_hunk(&mut self) {
        if let (Some(hunk), Some(file)) = (self.hunk.take(), self.file.as_mut()) {
            if !hunk.lines.is_empty() || !file.truncated {
                file.hunks.push(hunk);
            }
        }
        self.old_left = 0;
        self.new_left = 0;
    }

    fn flush_file(&mut self) {
        self.flush_hunk();
        let Some(file) = self.file.take() else {
            return;
        };
        self.changes.additions = self.changes.additions.saturating_add(file.additions);
        self.changes.deletions = self.changes.deletions.saturating_add(file.deletions);
        if file.truncated {
            self.changes.truncated = true;
        }
        if self.changes.files.len() < MAX_FILES {
            self.changes.files.push(file);
        } else {
            self.changes.omitted_files += 1;
            self.changes.truncated = true;
        }
    }

    fn line(&mut self, line: &str) {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            self.flush_file();
            let (old, new) = git_header_paths(rest);
            self.kept_file = 0;
            self.file = Some(ChangedFile {
                path: new.clone(),
                old_path: (old != new).then_some(old),
                ..ChangedFile::default()
            });
            return;
        }
        if self.file.is_none() {
            return;
        }
        if (self.old_left > 0 || self.new_left > 0) && self.hunk.is_some() {
            self.hunk_line(line);
            return;
        }
        if line.starts_with('\\') && self.hunk.is_some() {
            self.hunk_line(line);
            return;
        }
        if let Some((old_start, old_lines, new_start, new_lines)) = hunk_ranges(line) {
            self.flush_hunk();
            self.old_left = old_lines;
            self.new_left = new_lines;
            self.old_no = old_start;
            self.new_no = new_start;
            self.hunk = Some(Hunk {
                header: clip(line),
                old_start,
                old_lines,
                new_start,
                new_lines,
                lines: Vec::new(),
            });
            return;
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if line.starts_with("new file mode") {
            file.status = FileStatus::Added;
        } else if line.starts_with("deleted file mode") {
            file.status = FileStatus::Deleted;
        } else if let Some(from) = line.strip_prefix("rename from ") {
            file.status = FileStatus::Renamed;
            file.old_path = Some(unquote(from));
        } else if let Some(to) = line.strip_prefix("rename to ") {
            file.status = FileStatus::Renamed;
            file.path = unquote(to);
        } else if let Some(rest) = line.strip_prefix("--- ") {
            if header_path(rest, "a/").is_none() {
                file.status = FileStatus::Added;
            }
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            match header_path(rest, "b/") {
                Some(path) => file.path = path,
                None => file.status = FileStatus::Deleted,
            }
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            file.binary = true;
        }
    }

    fn hunk_line(&mut self, line: &str) {
        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Add, &line[1..]),
            Some(b'-') => (LineKind::Del, &line[1..]),
            Some(b'\\') => (LineKind::NoNewline, line),
            Some(b' ') => (LineKind::Context, &line[1..]),
            // An editor or a pipe stripped a context line's lone space.
            _ => (LineKind::Context, line),
        };
        let (old, new) = match kind {
            LineKind::Context => (Some(self.old_no), Some(self.new_no)),
            LineKind::Add => (None, Some(self.new_no)),
            LineKind::Del => (Some(self.old_no), None),
            LineKind::NoNewline => (None, None),
        };
        match kind {
            LineKind::Context => {
                self.old_no += 1;
                self.new_no += 1;
                self.old_left = self.old_left.saturating_sub(1);
                self.new_left = self.new_left.saturating_sub(1);
            }
            LineKind::Add => {
                self.new_no += 1;
                self.new_left = self.new_left.saturating_sub(1);
            }
            LineKind::Del => {
                self.old_no += 1;
                self.old_left = self.old_left.saturating_sub(1);
            }
            LineKind::NoNewline => {}
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        match kind {
            LineKind::Add => file.additions = file.additions.saturating_add(1),
            LineKind::Del => file.deletions = file.deletions.saturating_add(1),
            _ => {}
        }
        if self.kept_file >= MAX_FILE_LINES || self.kept_total >= MAX_TOTAL_LINES {
            file.truncated = true;
            return;
        }
        if let Some(hunk) = self.hunk.as_mut() {
            hunk.lines.push(DiffLine {
                kind,
                old,
                new,
                text: clip(text),
            });
            self.kept_file += 1;
            self.kept_total += 1;
        }
    }
}

impl Changes {
    /// `diff`, git's unified diff, as bounded data. `base` and `commit`
    /// are carried through.
    pub fn parse(base: &str, commit: Option<&str>, diff: &str) -> Changes {
        let mut parser = Parser::new(Changes {
            base: base.to_owned(),
            commit: commit.map(str::to_owned),
            ..Changes::default()
        });
        for line in diff.lines() {
            parser.line(line);
        }
        parser.flush_file();
        parser.changes
    }

    /// The file listed at `path`.
    pub fn file(&self, path: &str) -> Option<&ChangedFile> {
        self.files.iter().find(|f| f.path == path)
    }

    /// The text of the lines `start..=end` on `side` of `path`'s diff, in
    /// order; none when the file is not listed, binary, or any of the
    /// lines is not in its hunks (a line the diff does not show cannot be
    /// commented on).
    pub fn quote(&self, path: &str, side: DiffSide, start: u32, end: u32) -> Option<Vec<String>> {
        let file = self.file(path).filter(|f| !f.binary)?;
        let mut found: BTreeMap<u32, &str> = BTreeMap::new();
        for line in file.hunks.iter().flat_map(|h| &h.lines) {
            let number = match side {
                DiffSide::Old => line.old,
                DiffSide::New => line.new,
            };
            if let Some(number) = number.filter(|n| (start..=end).contains(n)) {
                found.insert(number, line.text.as_str());
            }
        }
        let count = usize::try_from(end - start).ok()? + 1;
        (found.len() == count).then(|| found.values().map(|t| (*t).to_owned()).collect())
    }
}

/// Why `comments` cannot be sent as a review of `changes`, or none. Each
/// comment needs text, a file the diff lists and lines it shows.
pub fn check_review(
    changes: &Changes,
    comments: &[ReviewComment],
    note: Option<&str>,
) -> Result<(), Error> {
    let usage = |message: String| Err(Error::Usage(message));
    if comments.is_empty() && note.is_none_or(|n| n.trim().is_empty()) {
        return usage("a review needs at least one comment or a note".into());
    }
    if comments.len() > MAX_REVIEW_COMMENTS {
        return usage(format!(
            "a review carries at most {MAX_REVIEW_COMMENTS} comments, not {}",
            comments.len()
        ));
    }
    if note.is_some_and(|n| n.chars().count() > MAX_REVIEW_TEXT) {
        return usage(format!(
            "the note is longer than {MAX_REVIEW_TEXT} characters"
        ));
    }
    for (index, comment) in comments.iter().enumerate() {
        let n = index + 1;
        if comment.body.trim().is_empty() {
            return usage(format!("comment {n} on {} has no text", comment.path));
        }
        if comment.body.chars().count() > MAX_REVIEW_TEXT {
            return usage(format!(
                "comment {n} is longer than {MAX_REVIEW_TEXT} characters"
            ));
        }
        let start = comment.start_line.unwrap_or(comment.line);
        if comment.line == 0 || start == 0 || start > comment.line {
            return usage(format!(
                "comment {n} on {}: lines are numbered from 1 and a range runs first to last",
                comment.path
            ));
        }
        if comment.line - start >= 1_000 {
            return usage(format!(
                "comment {n} on {} covers too many lines",
                comment.path
            ));
        }
        match changes.file(&comment.path) {
            None => {
                return usage(format!(
                    "comment {n}: {} is not a file this branch changes",
                    comment.path
                ))
            }
            Some(file) if file.binary => {
                return usage(format!("comment {n}: {} is a binary file", comment.path))
            }
            Some(_) => {}
        }
        if changes
            .quote(&comment.path, comment.side, start, comment.line)
            .is_none()
        {
            return usage(format!(
                "comment {n}: {} lines {start}-{} are not all in the diff; comment on lines the \
                 diff shows",
                comment.path, comment.line
            ));
        }
    }
    Ok(())
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

/// The prompt for a review: the format `by review` sends, with the quoted
/// lines of the diff under each comment. Deterministic. Call
/// [`check_review`] first; a comment whose lines the diff does not show is
/// written without a quotation.
pub fn review_prompt(changes: &Changes, comments: &[ReviewComment], note: Option<&str>) -> String {
    let files: BTreeSet<&str> = comments.iter().map(|c| c.path.as_str()).collect();
    let note = note.map(str::trim).filter(|n| !n.is_empty());
    let mut text = String::new();
    if comments.is_empty() {
        text.push_str("A review of this branch's changes, written on its diff in the console.\n");
    } else {
        text.push_str(&format!(
            "Review comments on this branch's changes ({} comment{} on {} file{}), written on \
             its diff in the console. A line number is the line in the file as this branch has \
             it; a comment marked \"Side: base\" is about lines this branch removed, numbered \
             as in the base.\n",
            comments.len(),
            if comments.len() == 1 { "" } else { "s" },
            files.len(),
            if files.len() == 1 { "" } else { "s" },
        ));
    }
    if let Some(note) = note {
        text.push_str(&format!("\nOverall note: \"{}\"\n", escape(note)));
    }
    for comment in comments {
        let start = comment.start_line.unwrap_or(comment.line);
        text.push_str(&format!("\nFile: {}\n", comment.path));
        if comment.side == DiffSide::Old {
            text.push_str("Side: base\n");
        }
        if start != comment.line {
            text.push_str(&format!("Lines: {start}-{}\n", comment.line));
        } else {
            text.push_str(&format!("Line: {}\n", comment.line));
        }
        if let Some(quoted) = changes.quote(&comment.path, comment.side, start, comment.line) {
            text.push_str("Quoted lines:\n");
            for line in quoted.iter().take(MAX_QUOTED) {
                text.push_str(&format!("> {line}\n"));
            }
            if quoted.len() > MAX_QUOTED {
                text.push_str(&format!("> … ({} more lines)\n", quoted.len() - MAX_QUOTED));
            }
        }
        text.push_str(&format!(
            "User comment: \"{}\"\n",
            escape(comment.body.trim())
        ));
    }
    if !comments.is_empty() {
        text.push_str("\nAddress each comment on this branch.\n");
    }
    text
}

/// The `(old_start, old_lines)` of each hunk of one file's `-U0` diff, or
/// none when the diff could not be made.
type OldRanges = Option<Vec<(u32, u32)>>;

/// The `(old_start, old_lines)` of every hunk in `diff`, a `-U0` diff of
/// one file.
fn old_ranges(diff: &str) -> Vec<(u32, u32)> {
    diff.lines()
        .filter_map(hunk_ranges)
        .map(|(start, lines, _, _)| (start, lines))
        .collect()
}

/// Whether a `-U0` hunk that replaces `old_lines` lines from `old_start`
/// touches the lines `start..=end`. An insertion (no old lines) sits
/// between lines `old_start` and the next.
fn touches(range: (u32, u32), start: u32, end: u32) -> bool {
    let (old_start, old_lines) = range;
    if old_lines == 0 {
        return start <= old_start && old_start < end;
    }
    let last = old_start.saturating_add(old_lines - 1);
    old_start <= end && start <= last
}

impl Branch {
    /// The candidate against the base, file by file and bounded; no files
    /// while there is no candidate. See [`Changes`].
    pub fn changes(&self) -> Result<Changes, Error> {
        let info = self.yard().store().read(&self.info().name)?.info;
        let Some(candidate) = &info.candidate else {
            return Ok(Changes {
                base: info.base,
                ..Changes::default()
            });
        };
        let diff = git::run(
            self.yard().root(),
            &[
                "-c",
                "core.quotepath=off",
                "diff",
                "--no-color",
                "--no-ext-diff",
                &info.base,
                &candidate.commit,
                "--",
            ],
        )?;
        Ok(Changes::parse(&info.base, Some(&candidate.commit), &diff))
    }

    /// The reviews sent to this branch, from `by review` and the console,
    /// read from its log. A comment sent against a candidate older than the
    /// current one is `addressed_in` the current candidate when the lines
    /// it names were changed since.
    pub fn reviews(&self) -> Result<Reviews, Error> {
        let current = self
            .yard()
            .store()
            .read(&self.info().name)?
            .info
            .candidate
            .map(|c| c.commit);
        let mut out = Reviews {
            candidate: current.clone(),
            reviews: Vec::new(),
        };
        for (index, event) in self.events()?.into_iter().enumerate() {
            let Activity::Feedback(feedback) = event.activity else {
                continue;
            };
            if feedback.source != FeedbackSource::Review {
                continue;
            }
            let mut note = None;
            let mut comments = Vec::new();
            for item in feedback.items {
                match item.path {
                    None => note = Some(item.body),
                    Some(path) => comments.push(SentComment {
                        path,
                        line: item.line.unwrap_or(0),
                        start_line: item.start_line,
                        side: item.side.unwrap_or_default(),
                        body: item.body,
                        addressed_in: None,
                    }),
                }
            }
            out.reviews.push(Review {
                id: index as u64 + 1,
                at_ms: event.at_ms,
                commit: feedback.reference,
                note,
                comments,
            });
        }
        if let Some(current) = current {
            self.mark_addressed(&mut out.reviews, &current);
        }
        Ok(out)
    }

    /// Set `addressed_in` on the comments of `reviews` whose lines the
    /// candidate `current` changed since the commit they were sent against.
    /// A commit git cannot compare leaves its comments unmarked.
    fn mark_addressed(&self, reviews: &mut [Review], current: &str) {
        let mut cache: BTreeMap<(String, String), OldRanges> = BTreeMap::new();
        for review in reviews {
            let Some(sent) = review.commit.clone().filter(|c| c != current) else {
                continue;
            };
            for comment in &mut review.comments {
                if comment.side != DiffSide::New || comment.line == 0 {
                    continue;
                }
                let ranges = cache
                    .entry((sent.clone(), comment.path.clone()))
                    .or_insert_with(|| {
                        git::run(
                            self.yard().root(),
                            &[
                                "-c",
                                "core.quotepath=off",
                                "diff",
                                "--no-color",
                                "--no-ext-diff",
                                "--no-renames",
                                "-U0",
                                &sent,
                                current,
                                "--",
                                &comment.path,
                            ],
                        )
                        .ok()
                        .map(|diff| old_ranges(&diff))
                    });
                let start = comment.start_line.unwrap_or(comment.line);
                if ranges
                    .as_ref()
                    .is_some_and(|r| r.iter().any(|range| touches(*range, start, comment.line)))
                {
                    comment.addressed_in = Some(current.to_owned());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,4 +1,5 @@ fn main
 one
-two
+TWO
+two and a half
 three
 four
\\ No newline at end of file
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..333
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+-- not a header
diff --git a/img.png b/img.png
index 444..555 100644
Binary files a/img.png and b/img.png differ
diff --git a/old name.txt b/moved.txt
similarity index 90%
rename from old name.txt
rename to moved.txt
index 666..777 100644
--- a/old name.txt
+++ b/moved.txt
@@ -5 +5 @@
-x
+y
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
index 888..0000000
--- a/gone.txt
+++ /dev/null
@@ -1 +0,0 @@
-bye
";

    #[test]
    fn a_unified_diff_becomes_numbered_files_and_hunks() {
        let changes = Changes::parse("base", Some("abc"), DIFF);
        assert_eq!(changes.files.len(), 5);
        assert_eq!((changes.additions, changes.deletions), (5, 3));
        let a = &changes.files[0];
        assert_eq!(
            (a.path.as_str(), a.additions, a.deletions),
            ("src/a.rs", 2, 1)
        );
        assert_eq!(a.status, FileStatus::Modified);
        let lines = &a.hunks[0].lines;
        assert_eq!(lines[0].old.zip(lines[0].new), Some((1, 1)));
        assert_eq!(
            (lines[1].kind, lines[1].old, lines[1].new),
            (LineKind::Del, Some(2), None)
        );
        assert_eq!(
            (lines[2].kind, lines[2].old, lines[2].new),
            (LineKind::Add, None, Some(2))
        );
        assert_eq!(lines[3].new, Some(3));
        assert_eq!(lines[4].old.zip(lines[4].new), Some((3, 4)));
        assert_eq!(lines.last().map(|l| l.kind), Some(LineKind::NoNewline));
        // A line that looks like a header inside a hunk is content.
        let new = &changes.files[1];
        assert_eq!(new.status, FileStatus::Added);
        assert_eq!(new.hunks[0].lines[1].text, "-- not a header");
        assert_eq!(new.hunks[0].lines[1].kind, LineKind::Add);
        let png = &changes.files[2];
        assert!(png.binary && png.hunks.is_empty());
        let moved = &changes.files[3];
        assert_eq!(moved.status, FileStatus::Renamed);
        assert_eq!(moved.old_path.as_deref(), Some("old name.txt"));
        assert_eq!(moved.path, "moved.txt");
        assert_eq!(changes.files[4].status, FileStatus::Deleted);
        assert!(!changes.truncated);
    }

    #[test]
    fn the_answer_is_bounded_and_says_what_it_left_out() {
        let mut diff = String::from("diff --git a/big b/big\n--- a/big\n+++ b/big\n");
        let lines = MAX_FILE_LINES + 50;
        diff.push_str(&format!("@@ -0,0 +1,{lines} @@\n"));
        for n in 0..lines {
            diff.push_str(&format!(
                "+{}\n",
                if n == 0 {
                    "x".repeat(MAX_LINE_CHARS + 5)
                } else {
                    n.to_string()
                }
            ));
        }
        for n in 0..MAX_FILES + 3 {
            diff.push_str(&format!(
                "diff --git a/f{n} b/f{n}\n--- a/f{n}\n+++ b/f{n}\n@@ -1 +1 @@\n-a\n+b\n"
            ));
        }
        let changes = Changes::parse("base", None, &diff);
        assert_eq!(changes.files.len(), MAX_FILES);
        assert_eq!(changes.omitted_files, 4);
        assert!(changes.truncated);
        let big = &changes.files[0];
        assert!(big.truncated);
        assert_eq!(big.additions as usize, lines);
        assert_eq!(big.hunks[0].lines.len(), MAX_FILE_LINES);
        assert!(big.hunks[0].lines[0].text.ends_with('…'));
        assert_eq!(changes.additions as usize, lines + MAX_FILES + 3);
    }

    #[test]
    fn quotes_come_from_the_lines_the_diff_shows() {
        let changes = Changes::parse("base", None, DIFF);
        assert_eq!(
            changes.quote("src/a.rs", DiffSide::New, 2, 3),
            Some(vec!["TWO".to_owned(), "two and a half".to_owned()])
        );
        assert_eq!(
            changes.quote("src/a.rs", DiffSide::Old, 2, 2),
            Some(vec!["two".to_owned()])
        );
        assert_eq!(changes.quote("src/a.rs", DiffSide::New, 4, 9), None);
        assert_eq!(changes.quote("img.png", DiffSide::New, 1, 1), None);
        assert_eq!(changes.quote("nope", DiffSide::New, 1, 1), None);
    }

    fn comment(path: &str, start: Option<u32>, line: u32, body: &str) -> ReviewComment {
        ReviewComment {
            path: path.into(),
            line,
            start_line: start,
            side: DiffSide::New,
            body: body.into(),
        }
    }

    #[test]
    fn a_review_is_one_prompt_naming_files_lines_and_their_text() {
        let changes = Changes::parse("base", None, DIFF);
        let comments = [
            comment("src/a.rs", Some(2), 3, "Why \"two\"?\nExplain."),
            comment("new.txt", None, 1, "ok"),
        ];
        check_review(&changes, &comments, Some("Mostly fine")).expect("valid");
        let prompt = review_prompt(&changes, &comments, Some("Mostly fine"));
        assert!(
            prompt.starts_with("Review comments on this branch's changes (2 comments on 2 files)")
        );
        assert!(prompt.contains("Overall note: \"Mostly fine\""));
        assert!(prompt.contains(
            "File: src/a.rs\nLines: 2-3\nQuoted lines:\n> TWO\n> two and a half\n\
             User comment: \"Why \\\"two\\\"?\\nExplain.\"\n"
        ));
        assert!(prompt.contains("File: new.txt\nLine: 1\nQuoted lines:\n> hello\n"));
        assert!(prompt.ends_with("Address each comment on this branch.\n"));
        let again = review_prompt(&changes, &comments, Some("Mostly fine"));
        assert_eq!(prompt, again);
        let old = ReviewComment {
            side: DiffSide::Old,
            ..comment("src/a.rs", None, 2, "why remove")
        };
        assert!(review_prompt(&changes, &[old], None)
            .contains("Side: base\nLine: 2\nQuoted lines:\n> two\n"));
    }

    #[test]
    fn a_review_that_cannot_be_placed_is_refused_with_the_reason() {
        let changes = Changes::parse("base", None, DIFF);
        let refused = |comments: &[ReviewComment], note: Option<&str>| {
            check_review(&changes, comments, note)
                .unwrap_err()
                .to_string()
        };
        assert!(refused(&[], None).contains("at least one comment"));
        assert!(refused(&[comment("src/a.rs", None, 1, " ")], None).contains("has no text"));
        assert!(refused(&[comment("zzz", None, 1, "x")], None)
            .contains("not a file this branch changes"));
        assert!(refused(&[comment("img.png", None, 1, "x")], None).contains("binary"));
        assert!(
            refused(&[comment("src/a.rs", None, 40, "x")], None).contains("not all in the diff")
        );
        assert!(refused(&[comment("src/a.rs", Some(3), 2, "x")], None).contains("first to last"));
        assert!(refused(&[comment("src/a.rs", None, 0, "x")], None).contains("numbered from 1"));
        check_review(&changes, &[], Some("just a note")).expect("a note alone is a review");
    }

    #[test]
    fn lines_a_later_candidate_changed_are_found_in_a_zero_context_diff() {
        let ranges = old_ranges("@@ -3,2 +3,0 @@\n-a\n-b\n@@ -9,0 +8,2 @@\n+x\n+y\n");
        assert_eq!(ranges, vec![(3, 2), (9, 0)]);
        assert!(touches((3, 2), 4, 6));
        assert!(!touches((3, 2), 5, 6));
        assert!(touches((9, 0), 8, 10), "an insertion inside the range");
        assert!(!touches((9, 0), 10, 12), "an insertion before the range");
    }

    #[test]
    fn git_quoted_and_spaced_paths_are_read() {
        assert_eq!(unquote("\"a\\tb\\\"c\\303\\244\""), "a\tb\"cä");
        assert_eq!(
            git_header_paths("a/dir/old name.txt b/dir/old name.txt"),
            ("dir/old name.txt".to_owned(), "dir/old name.txt".to_owned())
        );
        assert_eq!(
            git_header_paths("a/x b/y"),
            ("x".to_owned(), "y".to_owned())
        );
    }
}
