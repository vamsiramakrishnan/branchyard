//! `merge-yaml`: a narrow merge for block-style YAML, without a YAML
//! library (none is in the build). It reads only what it needs, by
//! indentation: block mappings (`key:` and `key: value`), block sequences
//! (`- item`), comments and blank lines. It merges by the same rules as the
//! other modes (a mapping gains missing keys, deeply; a sequence gains the
//! items it does not already hold, compared by their text with comments,
//! trailing spaces and indentation ignored) and it only ever inserts lines,
//! so every comment and every line of the person's file stays as it was.
//!
//! It refuses, with guidance to merge by hand, what it does not read:
//! tabs in indentation, several documents, a value with an anchor, alias
//! or tag where the fragment needs to enter it (elsewhere such values, like
//! `!!js` inside a sequence item, are copied and compared as text), and
//! a flow collection (`{...}`, `[...]`) or other value where the fragment
//! needs a mapping or a sequence.

#[derive(Clone, Copy)]
struct Line<'a> {
    indent: usize,
    /// The line less its indentation.
    text: &'a str,
    /// Neither blank nor a comment.
    significant: bool,
}

fn lines(text: &str) -> Result<Vec<Line<'_>>, String> {
    let mut out = Vec::new();
    let mut documents = 0;
    for raw in text.lines() {
        let body = raw.trim_start_matches(' ');
        let indent = raw.len() - body.len();
        let trimmed = body.trim_end();
        if body.starts_with('\t') {
            return Err("an indentation with tabs; merge it by hand".into());
        }
        if indent == 0 && (trimmed == "---" || trimmed.starts_with("--- ") || trimmed == "...") {
            documents += 1;
            if documents > 1 || trimmed == "..." || out.iter().any(|l: &Line| l.significant) {
                return Err("several YAML documents in one file; merge it by hand".into());
            }
            out.push(Line {
                indent,
                text: trimmed,
                significant: false,
            });
            continue;
        }
        out.push(Line {
            indent,
            text: trimmed,
            significant: !trimmed.is_empty() && !trimmed.starts_with('#'),
        });
    }
    Ok(out)
}

#[derive(Debug)]
enum Node {
    /// `key:` with nothing under it.
    Empty,
    /// A value on the key's line (and any lines under it), or a flow
    /// collection: compared by its text, never entered.
    Scalar(String),
    Map {
        indent: usize,
        entries: Vec<Entry>,
        last: usize,
    },
    Seq {
        indent: usize,
        items: Vec<(usize, usize)>,
        last: usize,
    },
}

#[derive(Debug)]
struct Entry {
    key: String,
    /// The key's line.
    line: usize,
    /// One past the entry's last line.
    end: usize,
    value: Node,
}

fn kind(node: &Node) -> &'static str {
    match node {
        Node::Empty => "empty",
        Node::Scalar(text) if tagged(text) => "a value with an anchor, alias or tag (not merged)",
        Node::Scalar(_) => "a value",
        Node::Map { .. } => "a mapping",
        Node::Seq { .. } => "a sequence",
    }
}

fn is_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

/// The key of a `key: ...` line and what follows the colon.
fn split_key(text: &str) -> Option<(String, &str)> {
    if let Some(quote) = text.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let close = text[1..].find(quote)? + 1;
        let rest = text[close + 1..].strip_prefix(':')?;
        if !(rest.is_empty() || rest.starts_with(' ')) {
            return None;
        }
        return Some((text[1..close].to_owned(), rest.trim()));
    }
    let colon = text
        .match_indices(':')
        .map(|(i, _)| i)
        .find(|i| text[i + 1..].is_empty() || text[i + 1..].starts_with(' '))?;
    let key = text[..colon].trim();
    (!key.is_empty() && !key.starts_with(['?', '{', '[', '&', '*', '!']))
        .then(|| (key.to_owned(), text[colon + 1..].trim()))
}

/// The next significant line at or after `from`, before `end`.
fn next_significant(lines: &[Line], from: usize, end: usize) -> Option<usize> {
    (from..end).find(|i| lines[*i].significant)
}

/// One past the last line belonging under a line at `indent` that starts
/// at `from`: up to the next significant line at `indent` or less.
fn block_end(lines: &[Line], from: usize, end: usize, indent: usize) -> usize {
    (from..end)
        .find(|i| lines[*i].significant && lines[*i].indent <= indent)
        .unwrap_or(end)
}

fn last_significant(lines: &[Line], start: usize, end: usize) -> usize {
    (start..end)
        .rev()
        .find(|i| lines[*i].significant)
        .unwrap_or(start)
}

/// The block in `start..end`, whose significant lines are all indented at
/// least as much as its first.
fn parse_block(lines: &[Line], start: usize, end: usize) -> Result<Node, String> {
    let Some(first) = next_significant(lines, start, end) else {
        return Ok(Node::Empty);
    };
    let indent = lines[first].indent;
    let last = last_significant(lines, start, end);
    if is_item(lines[first].text) {
        let mut items = Vec::new();
        let mut i = first;
        while i < end {
            let line = lines[i];
            if !line.significant {
                i += 1;
                continue;
            }
            if line.indent != indent || !is_item(line.text) {
                return Err(format!(
                    "line {}: a sequence mixed with other content; merge it by hand",
                    i + 1
                ));
            }
            let item_end = block_end(lines, i + 1, end, indent);
            items.push((i, item_end));
            i = item_end;
        }
        return Ok(Node::Seq {
            indent,
            items,
            last,
        });
    }
    let mut entries = Vec::new();
    let mut i = first;
    while i < end {
        let line = lines[i];
        if !line.significant {
            i += 1;
            continue;
        }
        if line.indent != indent {
            return Err(format!(
                "line {}: indentation this merge does not read; merge it by hand",
                i + 1
            ));
        }
        let Some((key, rest)) = split_key(line.text) else {
            return Err(format!(
                "line {}: not a `key:` line this merge reads; merge it by hand",
                i + 1
            ));
        };
        let rest = without_comment(rest);
        // A sequence may sit at its key's own indentation.
        let same_level_seq = rest.is_empty()
            && next_significant(lines, i + 1, end)
                .is_some_and(|n| lines[n].indent == indent && is_item(lines[n].text));
        let entry_end = match same_level_seq {
            true => (i + 1..end)
                .find(|n| {
                    lines[*n].significant
                        && (lines[*n].indent < indent
                            || (lines[*n].indent == indent && !is_item(lines[*n].text)))
                })
                .unwrap_or(end),
            false => block_end(lines, i + 1, end, indent),
        };
        let value = match rest {
            "" => parse_block(lines, i + 1, entry_end)?,
            rest => Node::Scalar(normalized(lines, i, entry_end, indent, Some(rest))),
        };
        entries.push(Entry {
            key,
            line: i,
            end: entry_end,
            value,
        });
        i = entry_end;
    }
    Ok(Node::Map {
        indent,
        entries,
        last,
    })
}

/// A value less a trailing ` # comment` outside quotes.
fn without_comment(value: &str) -> &str {
    let mut quote = None;
    let mut previous = ' ';
    for (i, c) in value.char_indices() {
        match (quote, c) {
            (None, '#') if previous == ' ' => return value[..i].trim_end(),
            (None, '"' | '\'') if i == 0 => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            _ => {}
        }
        previous = c;
    }
    value
}

/// Whether a value starts with an anchor, an alias or a tag: kept as text
/// where the merge passes it by (a tag such as `!!js` inside a sequence
/// item is copied as it is), refused where the merge would enter it.
fn tagged(value: &str) -> bool {
    value.starts_with(['&', '*', '!'])
}

/// A span's significant lines less `base` indentation, joined: how two
/// items or values are compared. With `first`, the first line is that text.
fn normalized(
    lines: &[Line],
    start: usize,
    end: usize,
    base: usize,
    first: Option<&str>,
) -> String {
    let mut out = Vec::new();
    for (n, line) in lines[start..end].iter().enumerate() {
        if !line.significant {
            continue;
        }
        match (n, first) {
            (0, Some(text)) => out.push(text.to_owned()),
            _ => out.push(format!(
                "{}{}",
                " ".repeat(line.indent.saturating_sub(base)),
                line.text
            )),
        }
    }
    out.join("\n")
}

/// The lines `start..end` of `raw`, re-indented by `to - from`, ending in
/// a newline.
fn reindent(raw: &[&str], start: usize, end: usize, from: usize, to: usize) -> String {
    let mut out = String::new();
    for line in &raw[start..end] {
        let body = line.trim_start_matches(' ');
        if body.trim().is_empty() {
            out.push('\n');
            continue;
        }
        let indent = (line.len() - body.len()).saturating_sub(from) + to;
        out.push_str(&" ".repeat(indent));
        out.push_str(body.trim_end());
        out.push('\n');
    }
    out
}

struct Merge<'a> {
    target: &'a [Line<'a>],
    fragment: &'a [Line<'a>],
    fragment_raw: &'a [&'a str],
    /// (insert before this target line, text), in the order made.
    inserts: Vec<(usize, String)>,
}

fn path_of(prefix: &str, key: &str) -> String {
    match prefix {
        "" => key.to_owned(),
        p => format!("{p}.{key}"),
    }
}

fn clash(path: &str, has: &str, wants: &str) -> String {
    let at = match path {
        "" => "the top level".to_owned(),
        p => format!("`{p}`"),
    };
    match has == wants {
        true => format!("{at} is already set to a different value; Branchyard does not replace it: change it by hand or remove it, then run again"),
        false => format!("{at} is {has} here but Branchyard's fragment has {wants} there; nothing is replaced: merge it by hand"),
    }
}

impl Merge<'_> {
    fn node(&mut self, target: &Node, fragment: &Node, path: &str) -> Result<(), String> {
        match (target, fragment) {
            (_, Node::Empty) => Ok(()),
            (
                Node::Map {
                    indent: t_indent,
                    entries: t_entries,
                    last,
                },
                Node::Map {
                    indent: f_indent,
                    entries: f_entries,
                    ..
                },
            ) => {
                for f in f_entries {
                    match t_entries.iter().find(|t| t.key == f.key) {
                        None => {
                            let text =
                                reindent(self.fragment_raw, f.line, f.end, *f_indent, *t_indent);
                            self.inserts.push((last + 1, text));
                        }
                        Some(t) => self.entry(t, *t_indent, f, &path_of(path, &f.key))?,
                    }
                }
                Ok(())
            }
            (
                Node::Seq {
                    indent: t_indent,
                    items: t_items,
                    last,
                },
                Node::Seq {
                    indent: f_indent,
                    items: f_items,
                    ..
                },
            ) => {
                let have: Vec<String> = t_items
                    .iter()
                    .map(|(s, e)| normalized(self.target, *s, *e, *t_indent, None))
                    .collect();
                for (s, e) in f_items {
                    let wanted = normalized(self.fragment, *s, *e, *f_indent, None);
                    if !have.contains(&wanted) {
                        let text = reindent(self.fragment_raw, *s, *e, *f_indent, *t_indent);
                        self.inserts.push((last + 1, text));
                    }
                }
                Ok(())
            }
            (Node::Scalar(a), Node::Scalar(b)) if a == b => Ok(()),
            (t, f) => Err(clash(path, kind(t), kind(f))),
        }
    }

    fn entry(&mut self, t: &Entry, t_indent: usize, f: &Entry, path: &str) -> Result<(), String> {
        if let (Node::Empty, Some(first)) =
            (&t.value, next_significant(self.fragment, f.line + 1, f.end))
        {
            // `key:` with nothing under it gains the fragment's block.
            let from = self.fragment[first].indent;
            let text = reindent(self.fragment_raw, f.line + 1, f.end, from, t_indent + 2);
            self.inserts.push((t.line + 1, text));
            return Ok(());
        }
        self.node(&t.value, &f.value, path)
    }
}

/// Merge the YAML `fragment` into `existing` under the dotted key `at`.
/// Returns `existing` byte for byte when it already holds everything.
pub fn merge(existing: Option<&str>, fragment: &str, at: &str) -> Result<String, String> {
    let mut wrapped = fragment.to_owned();
    for key in at.split('.').filter(|k| !k.is_empty()).rev() {
        let mut text = format!("{key}:\n");
        for line in wrapped.lines() {
            match line.trim().is_empty() {
                true => text.push('\n'),
                false => text.push_str(&format!("  {line}\n")),
            }
        }
        wrapped = text;
    }
    let fragment_lines = lines(&wrapped).map_err(|e| format!("the packaged fragment: {e}"))?;
    let fragment_node = parse_block(&fragment_lines, 0, fragment_lines.len())
        .map_err(|e| format!("the packaged fragment: {e}"))?;
    let existing_text = existing.unwrap_or("");
    let target_lines = lines(existing_text)?;
    if !target_lines.iter().any(|l| l.significant) {
        let mut out = existing_text.to_owned();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&wrapped);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        return Ok(out);
    }
    let target_node = parse_block(&target_lines, 0, target_lines.len())?;
    let fragment_raw: Vec<&str> = wrapped.lines().collect();
    let mut merge = Merge {
        target: &target_lines,
        fragment: &fragment_lines,
        fragment_raw: &fragment_raw,
        inserts: Vec::new(),
    };
    merge.node(&target_node, &fragment_node, "")?;
    if merge.inserts.is_empty() {
        return Ok(existing_text.to_owned());
    }
    let mut inserts = merge.inserts;
    inserts.sort_by_key(|(at, _)| *at);
    let raw: Vec<&str> = existing_text.lines().collect();
    let mut out = String::new();
    let mut pending = inserts.iter().peekable();
    for (i, line) in raw.iter().enumerate() {
        while let Some((_, text)) = pending.next_if(|(at, _)| *at == i) {
            out.push_str(text);
        }
        out.push_str(line);
        out.push('\n');
    }
    for (_, text) in pending {
        out.push_str(text);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HERMES: &str = "hooks:\n  on_session_start:\n    - command: \"by hook --harness hermes session-start\"\n      timeout: 5\n  pre_llm_call:\n    - command: \"by hook --harness hermes user-prompt-submit\"\n      timeout: 5\n";

    #[test]
    fn appends_under_a_top_level_key_keeping_every_line() {
        let user = "# my hermes\nmodel: gpt-5  # keep\nhooks:\n  on_session_start:\n  - command: mine\n    timeout: 2\n\ndisplay:\n  theme: dark\n";
        let once = merge(Some(user), HERMES, "").unwrap();
        assert_eq!(
            once,
            "# my hermes\nmodel: gpt-5  # keep\nhooks:\n  on_session_start:\n  - command: mine\n    timeout: 2\n  - command: \"by hook --harness hermes session-start\"\n    timeout: 5\n  pre_llm_call:\n    - command: \"by hook --harness hermes user-prompt-submit\"\n      timeout: 5\n\ndisplay:\n  theme: dark\n"
        );
        assert_eq!(merge(Some(&once), HERMES, "").unwrap(), once);
    }

    #[test]
    fn creates_or_appends_a_missing_key_and_fills_an_empty_one() {
        assert_eq!(merge(None, HERMES, "").unwrap(), HERMES);
        let added = merge(Some("model: x\n"), HERMES, "").unwrap();
        assert_eq!(added, format!("model: x\n{HERMES}"));
        let empty = merge(Some("hooks:\nmodel: x\n"), HERMES, "").unwrap();
        assert!(
            empty.starts_with("hooks:\n  on_session_start:\n"),
            "{empty}"
        );
        assert!(empty.ends_with("model: x\n"), "{empty}");
        assert_eq!(merge(Some(&empty), HERMES, "").unwrap(), empty);
    }

    #[test]
    fn appends_a_top_level_sequence_item_once() {
        let row = "- name: '@deepseek-ai/dsh-hooks-claude-code'\n  config:\n    configPath: ~/.dsh/branchyard/hooks.json\n";
        let user = "# patch\n- insert:\n    - id: mine\n";
        let once = merge(Some(user), row, "").unwrap();
        assert_eq!(once, format!("{user}{row}"));
        assert_eq!(merge(Some(&once), row, "").unwrap(), once);
    }

    #[test]
    fn a_tagged_item_is_copied_as_it_is_and_once() {
        // DeepSeek Harness's patch row: a top-level list, a `!!js` tag and
        // a folded scalar inside the item, comments before it.
        let row = "# Branchyard's row\n- insert:\n    - id: branchyard-hooks\n      name: '@deepseek-ai/dsh-hooks-claude-code'\n      config:\n        configPath: !!js >-\n          process.getBuiltinModule('node:os').homedir()\n";
        assert_eq!(merge(None, row, "").unwrap(), row);
        let user = "- insert:\n    - id: mine\n      config:\n        x: !!js 1\n";
        let once = merge(Some(user), row, "").unwrap();
        assert_eq!(
            once,
            format!("{user}{}", &row[row.find("- insert").unwrap()..])
        );
        assert_eq!(merge(Some(&once), row, "").unwrap(), once);
    }

    #[test]
    fn at_nests_the_fragment() {
        let out = merge(Some("a:\n  x: 1\n"), "y: 2\n", "a").unwrap();
        assert_eq!(out, "a:\n  x: 1\n  y: 2\n");
    }

    #[test]
    fn refuses_what_it_does_not_read() {
        for (user, wanted) in [
            (
                "hooks: {}\n",
                "`hooks` is a value here but Branchyard's fragment has a mapping",
            ),
            (
                "hooks:\n  on_session_start: []\n",
                "`hooks.on_session_start` is a value",
            ),
            ("hooks:\n\ton_session_start:\n", "tabs"),
            ("a: 1\n---\nb: 2\n", "several YAML documents"),
            ("hooks: &h\n  x: 1\n", "an anchor, alias or tag"),
            ("hooks: !!map\n  x: 1\n", "an anchor, alias or tag"),
            ("- a\nb: 1\n", "sequence mixed"),
        ] {
            let e = merge(Some(user), HERMES, "").unwrap_err();
            assert!(e.contains(wanted), "{user:?}: {e}");
        }
        let e = merge(Some("model: a\n"), "model: b\n", "").unwrap_err();
        assert!(e.contains("`model` is already set"), "{e}");
        assert_eq!(
            merge(Some("model: a  # c\n"), "model: a\n", "").unwrap(),
            "model: a  # c\n"
        );
    }
}
