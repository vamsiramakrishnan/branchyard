// Derived from stablyai/orca src/shared/diff-comments-format.ts and
// src/shared/diff-comment-types.ts, at revision
// 280733273545f0b3eeedc1be54b14d406239030e.
// Copyright (c) 2026 Lovecast Inc. Licensed under the MIT License; the
// license text, which must accompany substantial portions of this code, is
// in vendor/orca/LICENSE.
// Modified for Branchyard: translated from TypeScript to Rust; the comment
// keeps only the fields the format reads (file, optional start line, line,
// body), and Orca's markdown-note variant (`Source: markdown`), which `by
// review` never produces, is left out. The format itself, its escaping and
// its location labels are Orca's, so an agent that reads Orca's review notes
// reads these the same way.

//! The text `by review` sends: each comment as `File:`, a location
//! (`Scope: file`, `Line: N` or `Lines: A-B`) and `User comment: "..."`,
//! quoted and escaped so it stays one line, with a blank line between
//! comments. Deterministic: the same comments always give the same bytes.

/// A comment on the branch's side of its diff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffComment {
    /// The file's path in the branch.
    pub file_path: String,
    /// The first line of an inclusive range, when the comment covers more
    /// than one; at most `line_number`.
    pub start_line: Option<u32>,
    /// The (last) line it is about, or 0 for the whole file.
    pub line_number: u32,
    pub body: String,
}

/// One comment, as Orca's `formatDiffComment` writes it.
pub fn format_diff_comment(c: &DiffComment) -> String {
    let escaped = c
        .body
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n");
    let location = match (c.line_number, c.start_line) {
        (0, _) => "Scope: file".to_owned(),
        (line, Some(start)) if start != line => format!("Lines: {start}-{line}"),
        (line, _) => format!("Line: {line}"),
    };
    [
        format!("File: {}", c.file_path),
        location,
        format!("User comment: \"{escaped}\""),
    ]
    .join("\n")
}

/// Several comments, as Orca's `formatDiffComments` joins them.
pub fn format_diff_comments(comments: &[DiffComment]) -> String {
    comments
        .iter()
        .map(format_diff_comment)
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(start: Option<u32>, line: u32, body: &str) -> DiffComment {
        DiffComment {
            file_path: "src/a.rs".into(),
            start_line: start,
            line_number: line,
            body: body.into(),
        }
    }

    /// The cases Orca's format distinguishes: a line, a range, a range of
    /// one line, the whole file, and a body that needs escaping.
    #[test]
    fn comments_format_as_orca_formats_them() {
        assert_eq!(
            format_diff_comment(&comment(None, 12, "Use a constant")),
            "File: src/a.rs\nLine: 12\nUser comment: \"Use a constant\""
        );
        assert_eq!(
            format_diff_comment(&comment(Some(3), 7, "x")),
            "File: src/a.rs\nLines: 3-7\nUser comment: \"x\""
        );
        assert_eq!(
            format_diff_comment(&comment(Some(7), 7, "x")),
            "File: src/a.rs\nLine: 7\nUser comment: \"x\""
        );
        assert_eq!(
            format_diff_comment(&comment(Some(1), 0, "x")),
            "File: src/a.rs\nScope: file\nUser comment: \"x\""
        );
        assert_eq!(
            format_diff_comment(&comment(None, 1, "say \"hi\"\\\r\nnext")),
            "File: src/a.rs\nLine: 1\nUser comment: \"say \\\"hi\\\"\\\\\\r\\nnext\""
        );
        assert_eq!(
            format_diff_comments(&[comment(None, 1, "a"), comment(None, 2, "b")]),
            "File: src/a.rs\nLine: 1\nUser comment: \"a\"\n\n\
             File: src/a.rs\nLine: 2\nUser comment: \"b\""
        );
        assert_eq!(format_diff_comments(&[]), "");
    }
}
