// Derived from Scion (https://github.com/GoogleCloudPlatform/scion) at
// d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338: _strip_managed_block,
// _markdown_section and project_instructions of harnesses/scion_harness.py.
// Copyright 2026 Google LLC. Licensed under the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to Rust as pure functions
// over the file's content (the caller reads and writes it); Branchyard's
// own markers are written, and Scion's are still recognized and replaced;
// a begin marker without an end marker makes merge_block refuse, where
// Scion would add a second block; skills are not inlined, because no
// translated harness needs it.

//! Standing instructions kept in a harness's instruction file between
//! markers, so what a person wrote outside them stays.

/// The markers Branchyard writes.
pub const BEGIN: &str = "<!-- BEGIN BRANCHYARD MANAGED -->";
pub const END: &str = "<!-- END BRANCHYARD MANAGED -->";

/// Begin markers recognized, in the order they are looked for: Branchyard's,
/// then Scion's.
const BEGIN_MARKERS: &[&str] = &[
    BEGIN,
    "<!-- BEGIN SCION MANAGED CODEX INSTRUCTIONS -->",
    "<!-- BEGIN SCION MANAGED HERMES INSTRUCTIONS -->",
    "<!-- SCION_MANAGED_BEGIN -->",
    "<!-- BEGIN SCION MANAGED -->",
];

const END_MARKERS: &[&str] = &[
    END,
    "<!-- END SCION MANAGED CODEX INSTRUCTIONS -->",
    "<!-- END SCION MANAGED HERMES INSTRUCTIONS -->",
    "<!-- SCION_MANAGED_END -->",
    "<!-- END SCION MANAGED -->",
];

/// Where a managed block is.
enum Found {
    None,
    Block(usize, usize),
    /// A begin marker at this offset with no end marker after it.
    Unclosed,
}

fn find_block(content: &str) -> Found {
    let Some((start, begin)) = BEGIN_MARKERS
        .iter()
        .find_map(|m| content.find(m).map(|i| (i, *m)))
    else {
        return Found::None;
    };
    let after = start + begin.len();
    match END_MARKERS
        .iter()
        .find_map(|m| content[after..].find(m).map(|i| after + i + m.len()))
    {
        Some(end) => Found::Block(start, end),
        None => Found::Unclosed,
    }
}

/// `content` without its managed block. Content with a begin marker but no
/// end marker is returned unchanged, so nothing is lost.
pub fn strip_managed_block(content: &str) -> String {
    match find_block(content) {
        Found::Block(start, end) => {
            format!(
                "{}\n",
                format!("{}{}", &content[..start], &content[end..]).trim()
            )
        }
        Found::None | Found::Unclosed => content.to_owned(),
    }
}

/// `# title`, a blank line and the trimmed body; empty for an empty body.
pub fn section(title: &str, body: &str) -> String {
    match body.trim() {
        "" => String::new(),
        body => format!("# {title}\n\n{body}\n"),
    }
}

/// The managed block's body from sections, as Scion composes them.
pub fn compose(sections: &[String]) -> Option<String> {
    let parts: Vec<&str> = sections
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// Replace the managed block of `current` with `body` (none: remove it).
/// The block comes first and user content after it. Returns `None` when
/// nothing is left, so the file is removed.
pub fn merge_block(current: Option<&str>, body: Option<&str>) -> Result<Option<String>, String> {
    let current = current.unwrap_or("");
    if matches!(find_block(current), Found::Unclosed) {
        return Err(
            "has a managed-block begin marker without an end marker; not editing it".into(),
        );
    }
    let existing = strip_managed_block(current);
    let managed = body
        .filter(|b| !b.trim().is_empty())
        .map(|b| format!("{BEGIN}\n\n{}\n\n{END}\n", b.trim()));
    let unmanaged = match existing.trim() {
        "" => None,
        text => Some(format!("{text}\n")),
    };
    let content = match (managed, unmanaged) {
        (None, None) => return Ok(None),
        (Some(managed), None) => managed,
        (None, Some(unmanaged)) => unmanaged,
        (Some(managed), Some(unmanaged)) => format!("{managed}\n{unmanaged}"),
    };
    // Unchanged content is returned exactly.
    Ok(Some(if content == current {
        current.to_owned()
    } else {
        content
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEGACY_BEGIN: &str = "<!-- BEGIN SCION MANAGED CODEX INSTRUCTIONS -->";
    const LEGACY_END: &str = "<!-- END SCION MANAGED CODEX INSTRUCTIONS -->";

    // From Scion's scion_harness_test.py, TestStripManagedBlock.

    #[test]
    fn strip_standard_markers() {
        let content =
            "before\n<!-- BEGIN SCION MANAGED -->\nmanaged\n<!-- END SCION MANAGED -->\nafter";
        let result = strip_managed_block(content);
        assert!(result.contains("before") && result.contains("after"));
        assert!(!result.contains("managed"));
    }

    #[test]
    fn strip_codex_legacy_markers() {
        let content = format!("user\n{LEGACY_BEGIN}\nstuff\n{LEGACY_END}\nrest");
        let result = strip_managed_block(&content);
        assert!(result.contains("user") && result.contains("rest"));
        assert!(!result.contains("stuff"));
    }

    #[test]
    fn strip_hermes_and_copilot_legacy_markers() {
        for (begin, end) in [
            (
                "<!-- BEGIN SCION MANAGED HERMES INSTRUCTIONS -->",
                "<!-- END SCION MANAGED HERMES INSTRUCTIONS -->",
            ),
            ("<!-- SCION_MANAGED_BEGIN -->", "<!-- SCION_MANAGED_END -->"),
            (BEGIN, END),
        ] {
            let content = format!("user\n{begin}\nstuff\n{end}\nrest");
            assert!(!strip_managed_block(&content).contains("stuff"), "{begin}");
        }
    }

    #[test]
    fn unclosed_marker_preserved() {
        let content = "before\n<!-- BEGIN SCION MANAGED -->\nunclosed content";
        assert_eq!(strip_managed_block(content), content);
        // Scion would prepend a second block; Branchyard refuses.
        assert!(merge_block(Some(content), Some("new")).is_err());
    }

    #[test]
    fn no_markers() {
        assert!(strip_managed_block("just plain text\n").contains("just plain text"));
    }

    // From Scion's TestProjectInstructions and codex/provision_test.py.

    #[test]
    fn composes_sections_once_however_often_it_runs() {
        let body = compose(&[
            section("System Instruction", "System rules"),
            section("Agent Instructions", "Agent rules"),
        ])
        .unwrap();
        let once = merge_block(None, Some(&body)).unwrap().unwrap();
        let twice = merge_block(Some(&once), Some(&body)).unwrap().unwrap();
        assert_eq!(once, twice);
        assert_eq!(twice.matches(BEGIN).count(), 1);
        assert!(twice.contains(
            "# System Instruction\n\nSystem rules\n\n# Agent Instructions\n\nAgent rules"
        ));
        assert!(twice.contains(END));
    }

    #[test]
    fn strips_existing_managed_block() {
        let current =
            "<!-- BEGIN SCION MANAGED -->\nold\n<!-- END SCION MANAGED -->\nuser content\n";
        let body = compose(&[section("Agent Instructions", "New instructions.")]).unwrap();
        let out = merge_block(Some(current), Some(&body)).unwrap().unwrap();
        assert!(!out.contains("old"));
        assert!(out.contains("New instructions.") && out.contains("user content"));
        assert!(out.starts_with(BEGIN));
    }

    #[test]
    fn cleans_stale_managed_block_when_inputs_empty() {
        let current = format!(
            "{LEGACY_BEGIN}\n\n# Agent Instructions\n\nOld managed content\n\n{LEGACY_END}\n\n\
             # User Notes\n\nKeep this.\n"
        );
        let out = merge_block(Some(&current), None).unwrap().unwrap();
        assert_eq!(out, "# User Notes\n\nKeep this.\n");
    }

    #[test]
    fn removes_file_when_only_stale_managed_block_remains() {
        let current = format!(
            "{LEGACY_BEGIN}\n\n# Agent Instructions\n\nOld managed content\n\n{LEGACY_END}\n"
        );
        assert_eq!(merge_block(Some(&current), None).unwrap(), None);
        assert_eq!(merge_block(None, None).unwrap(), None);
    }
}
