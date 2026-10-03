// Derived from stablyai/orca src/main/jira/adf-markdown.ts and
// src/main/jira/adf-media-destination.ts, at revision
// 280733273545f0b3eeedc1be54b14d406239030e.
// Copyright (c) 2026 Lovecast Inc. Licensed under the MIT License; the
// license text, which must accompany substantial portions of this code, is
// in vendor/orca/LICENSE.
// Modified for Branchyard: translated from TypeScript to Rust over
// serde_json values; the media resolver option (Orca downloads Jira
// attachments) is left out, so media becomes an image link when it has an
// http(s) URL and a visible placeholder otherwise; `textToAdf` and
// `collectAdfMediaAttrs` (Orca's writing and attachment paths) are not
// ported.

//! Jira's Atlassian Document Format (a description in Jira REST v3) as
//! Markdown, for the prompt `by run --issue jira:KEY` builds.

use serde_json::Value;

struct Block {
    list: bool,
    text: String,
}

fn block(text: String) -> Block {
    Block { list: false, text }
}

fn text_of(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

fn positive(value: &Value, fallback: u64) -> u64 {
    match value.as_u64() {
        Some(n) if n > 0 => n,
        _ => fallback,
    }
}

/// Orca's `escapeMarkdownAlt`.
fn escape_alt(text: &str) -> String {
    text.chars().filter(|c| *c != '[' && *c != ']').collect()
}

/// Orca's `escapeMarkdownLinkDestination`: percent-encode what would end or
/// break a Markdown link destination, without double-encoding `%HH`.
fn escape_destination(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return None;
    }
    let hostile = |c: char| "()[]<>\"'`\\".contains(c) || (c as u32) <= 0x20;
    let mut out = String::new();
    let chars: Vec<char> = url.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '%' {
            let hex: String = chars[i + 1..chars.len().min(i + 3)].iter().collect();
            if hex.len() == 2 && hex.chars().all(|h| h.is_ascii_hexdigit()) {
                out.push('%');
                out.push_str(&hex);
                i += 3;
                continue;
            }
            out.push_str("%25");
            i += 1;
            continue;
        }
        if hostile(c) {
            let mut buf = [0u8; 4];
            for byte in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    (!out.chars().any(hostile)).then_some(out)
}

fn media(record: &Value) -> String {
    let attrs = &record["attrs"];
    let alt = [text_of(&attrs["alt"]), text_of(&attrs["name"])]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .trim();
    let label = escape_alt(if alt.is_empty() { "Image" } else { alt });
    let url = text_of(&attrs["url"]);
    // Orca's `unresolvedMediaPlaceholder`: keep a visible marker, so a
    // screenshot is not silently dropped from the issue.
    match escape_destination(url) {
        Some(url) => format!("![{label}]({url})"),
        None => format!("*[{label}]*"),
    }
}

fn inline(node: &Value) -> String {
    match node {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(inline).collect(),
        Value::Object(record) => {
            if let Some(text) = record.get("text").and_then(Value::as_str) {
                return text.to_owned();
            }
            match record.get("type").and_then(Value::as_str) {
                Some("hardBreak") => return "\n".into(),
                Some("media" | "mediaInline") => return media(node),
                _ => {}
            }
            let attrs = &node["attrs"];
            let fallback = [
                text_of(&attrs["text"]),
                text_of(&attrs["shortName"]),
                text_of(&attrs["url"]),
            ]
            .into_iter()
            .find(|s| !s.is_empty());
            match fallback {
                Some(text) => text.to_owned(),
                None => inline(&node["content"]),
            }
        }
        _ => String::new(),
    }
}

fn join(blocks: &[Block]) -> String {
    blocks
        .iter()
        .map(|b| b.text.as_str())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn blocks(content: &Value) -> Vec<Block> {
    content
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(render)
                .filter(|b| !b.text.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn list_item(node: &Value, prefix: &str) -> String {
    let inner = blocks(&node["content"]);
    if inner.is_empty() {
        return prefix.trim_end().to_owned();
    }
    let indent = " ".repeat(prefix.chars().count());
    let mut lines: Vec<String> = Vec::new();
    for (index, block) in inner.iter().enumerate() {
        let mut text_lines = block.text.split('\n');
        if index == 0 {
            let first = text_lines.next().unwrap_or("");
            lines.push(format!("{prefix}{first}").trim_end().to_owned());
            for line in text_lines {
                lines.push(format!("{indent}{line}").trim_end().to_owned());
            }
            continue;
        }
        if !block.list {
            lines.push(String::new());
        }
        for line in text_lines {
            lines.push(format!("{indent}{line}").trim_end().to_owned());
        }
    }
    lines.join("\n")
}

fn list(record: &Value, ordered: bool) -> String {
    let start = if ordered {
        positive(&record["attrs"]["order"], 1)
    } else {
        1
    };
    record["content"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .enumerate()
                .map(|(i, item)| match ordered {
                    true => list_item(item, &format!("{}. ", start + i as u64)),
                    false => list_item(item, "- "),
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn render(node: &Value) -> Block {
    let record = match node {
        Value::String(s) => return block(s.clone()),
        Value::Array(_) => return block(join(&blocks(node))),
        Value::Object(_) => node,
        _ => return block(String::new()),
    };
    match text_of(&record["type"]) {
        "doc" => block(join(&blocks(&record["content"]))),
        "paragraph" => block(inline(&record["content"])),
        "heading" => {
            let level = positive(&record["attrs"]["level"], 1).clamp(1, 6) as usize;
            block(
                format!(
                    "{} {}",
                    "#".repeat(level),
                    inline(&record["content"]).trim()
                )
                .trim()
                .to_owned(),
            )
        }
        // Orca renders Jira bodies as Markdown, so list containers need
        // concrete markers rather than flattened lines.
        "bulletList" => Block {
            list: true,
            text: list(record, false),
        },
        "orderedList" => Block {
            list: true,
            text: list(record, true),
        },
        "listItem" => Block {
            list: true,
            text: list_item(record, "- "),
        },
        "codeBlock" => {
            let text = inline(&record["content"]);
            let text = text.strip_suffix('\n').unwrap_or(&text);
            block(format!("```\n{text}\n```"))
        }
        "blockquote" => block(
            join(&blocks(&record["content"]))
                .split('\n')
                .map(|line| format!("> {line}").trim_end().to_owned())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        "rule" => block("---".into()),
        "mediaSingle" | "mediaGroup" => block(join(&blocks(&record["content"]))),
        "media" | "mediaInline" => block(media(record)),
        _ => {
            let inner = join(&blocks(&record["content"]));
            block(match inner.is_empty() {
                true => inline(record),
                false => inner,
            })
        }
    }
}

/// Orca's `adfToMarkdownText`: the document as Markdown, trailing spaces
/// before line breaks removed and runs of blank lines folded to one.
pub fn to_markdown(value: &Value) -> String {
    let text = render(value).text;
    let mut lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.trim_end_matches([' ', '\t']))
        .collect();
    // Keep the last line's trailing spaces as Orca does (only those before
    // a line break are removed); the final trim removes them anyway.
    let mut out = String::new();
    let mut blank = 0;
    for line in lines.drain(..) {
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_document_becomes_markdown() {
        let doc = json!({"type": "doc", "version": 1, "content": [
            {"type": "heading", "attrs": {"level": 2}, "content": [{"type": "text", "text": "Steps"}]},
            {"type": "paragraph", "content": [
                {"type": "text", "text": "Open the "}, {"type": "text", "text": "parser"},
                {"type": "hardBreak"}, {"type": "text", "text": "then crash"}]},
            {"type": "orderedList", "attrs": {"order": 3}, "content": [
                {"type": "listItem", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "one"}]}]},
                {"type": "listItem", "content": [
                    {"type": "paragraph", "content": [{"type": "text", "text": "two"}]},
                    {"type": "bulletList", "content": [
                        {"type": "listItem", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "nested"}]}]}]}]}]},
            {"type": "codeBlock", "content": [{"type": "text", "text": "cargo test\n"}]},
            {"type": "blockquote", "content": [{"type": "paragraph", "content": [{"type": "text", "text": "quoted"}]}]},
            {"type": "rule"},
            {"type": "mediaSingle", "content": [{"type": "media", "attrs": {"url": "https://x.test/a (1).png", "alt": "shot [1]"}}]},
            {"type": "mediaSingle", "content": [{"type": "media", "attrs": {"id": "abc", "alt": "local"}}]},
            {"type": "paragraph", "content": [{"type": "mention", "attrs": {"text": "@ada"}}]}
        ]});
        assert_eq!(
            to_markdown(&doc),
            "## Steps\n\nOpen the parser\nthen crash\n\n3. one\n4. two\n   - nested\n\n\
             ```\ncargo test\n```\n\n> quoted\n\n---\n\n![shot 1](https://x.test/a%20%281%29.png)\n\n\
             *[local]*\n\n@ada"
        );
    }

    /// The vendored source still has the shape this port follows.
    #[test]
    fn orcas_source_is_the_one_ported() {
        let source = include_str!("../../../vendor/orca/src/main/jira/adf-markdown.ts");
        for needle in [
            "export function adfToMarkdownText",
            ".replace(/[ \\t]+\\n/g, '\\n')",
            ".replace(/\\n{3,}/g, '\\n\\n')",
            "if (type === 'bulletList') {",
            "return `*[${label}]*`",
        ] {
            assert!(
                source.contains(needle),
                "adf-markdown.ts no longer has {needle}"
            );
        }
    }

    #[test]
    fn hostile_destinations_are_encoded_or_refused() {
        assert_eq!(
            escape_destination("https://a.test/x%2Fy%zz").as_deref(),
            Some("https://a.test/x%2Fy%25zz")
        );
        assert_eq!(escape_destination("javascript:alert(1)"), None);
        assert_eq!(to_markdown(&json!(null)), "");
        assert_eq!(to_markdown(&json!("plain")), "plain");
    }
}
