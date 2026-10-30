//! Skills as data: what makes a directory a skill, a skill's digest, the
//! digest of a set of skills, and the capability index's generated
//! "Custom" topic. Nothing here does I/O.
//!
//! A skill is a directory whose `SKILL.md` opens with YAML frontmatter
//! that names it (`name`, which must be the directory's name: lowercase
//! letters, digits and hyphens, at most 64) and says what it is for
//! (`description`), plus any supporting files. It holds at most
//! [`MAX_SKILL_FILES`] files and [`MAX_SKILL_BYTES`] bytes, all UTF-8 text,
//! at relative paths with no `.`, `..` or empty part. Line endings may be
//! CRLF. Whoever reads a skill's files refuses symbolic links.
//!
//! A skill's digest is BLAKE3 over its files' relative paths and contents
//! in path order; a set's is BLAKE3 over its `(layer, name, digest)`
//! entries in layer and name order. Each field is length-prefixed, so no
//! two different inputs hash alike by running into each other. Both are
//! lowercase hex. See `docs/knowledge-skills.md`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The most bytes one skill's files may hold together.
pub const MAX_SKILL_BYTES: u64 = 1 << 20;

/// The most files one skill may hold.
pub const MAX_SKILL_FILES: usize = 200;

/// The most bytes the capability index's root may hold.
pub const ROOT_INDEX_BUDGET: usize = 4096;

/// The most bytes a topic's index may hold.
pub const DETAIL_INDEX_BUDGET: usize = 8192;

/// Where repository skills are staged, relative to the capability root,
/// beside the built-in skills.
pub const REPOSITORY_ROOT: &str = "repository";

/// The generated topic that lists custom skills, relative to the
/// capability root.
pub const CUSTOM_TOPIC: &str = "topics/custom/INDEX.md";

/// The longest skill name.
const MAX_NAME: usize = 64;

/// Where a skill came from, most specific first.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    /// Committed to the repository under `.agents/skills/`, read from the
    /// branch's base commit.
    Repository,
    /// Shipped in the `by` binary.
    BuiltIn,
}

impl Layer {
    /// The layer as serialized: `repository`, `built_in`.
    pub fn as_str(self) -> &'static str {
        match self {
            Layer::Repository => "repository",
            Layer::BuiltIn => "built_in",
        }
    }
}

/// One skill of a set: where it came from, its name and its digest.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillEntry {
    /// Where it came from.
    pub layer: Layer,
    /// Its directory's name.
    pub name: String,
    /// BLAKE3 over its files' relative paths and contents ([`digest`]).
    pub digest: String,
}

impl SkillEntry {
    /// How an index and a person name it: a built-in skill by its name, a
    /// custom one under its layer, as `repository:style-guide`.
    pub fn label(&self) -> String {
        match self.layer {
            Layer::BuiltIn => self.name.clone(),
            layer => format!("{}:{}", layer.as_str(), self.name),
        }
    }
}

/// A checked skill: its name and description and its files by relative
/// path, in path order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    /// Its directory's name, which its frontmatter's `name` matches.
    pub name: String,
    /// Its frontmatter's `description`, on one line.
    pub description: String,
    /// Relative path and content of each file, in path order.
    pub files: Vec<(String, String)>,
    /// [`digest`] of its files.
    pub digest: String,
}

impl Skill {
    /// Check the files of the skill directory `name` (each path relative to
    /// it) and compute its digest: the limits, the paths, and a `SKILL.md`
    /// whose frontmatter names `name` and gives a description.
    pub fn new(name: &str, mut files: Vec<(String, String)>) -> Result<Skill, String> {
        check_listing(
            name,
            files
                .iter()
                .map(|(path, text)| (path.as_str(), text.len() as u64)),
        )?;
        files.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some(pair) = files.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(format!("skill {name} lists {} twice", pair[0].0));
        }
        let Some((_, text)) = files.iter().find(|(path, _)| path == "SKILL.md") else {
            return Err(format!("skill {name} has no SKILL.md"));
        };
        let front = frontmatter(text).map_err(|why| format!("skill {name}: SKILL.md {why}"))?;
        if front.name != name {
            return Err(format!(
                "skill {name}: SKILL.md names {:?}; the name must be its directory's",
                front.name
            ));
        }
        let description = one_line(&front.description);
        if description.chars().any(char::is_control) {
            return Err(format!(
                "skill {name}: its description holds a control character, which would reach a \
                 terminal that shows it"
            ));
        }
        let digest = digest(
            files
                .iter()
                .map(|(path, text)| (path.as_str(), text.as_bytes())),
        );
        Ok(Skill {
            name: name.to_owned(),
            description,
            files,
            digest,
        })
    }

    /// Its entry in a set, in `layer`.
    pub fn entry(&self, layer: Layer) -> SkillEntry {
        SkillEntry {
            layer,
            name: self.name.clone(),
            digest: self.digest.clone(),
        }
    }
}

/// Check what a skill directory `name` holds before reading its files:
/// its name, how many files and bytes, and each relative path.
pub fn check_listing<'a>(
    name: &str,
    files: impl IntoIterator<Item = (&'a str, u64)>,
) -> Result<(), String> {
    check_name(name)?;
    let (mut count, mut bytes) = (0usize, 0u64);
    for (path, size) in files {
        check_path(path).map_err(|why| format!("skill {name}: {why}"))?;
        count += 1;
        bytes = bytes.saturating_add(size);
    }
    if count > MAX_SKILL_FILES {
        return Err(format!(
            "skill {name} has {count} files; a skill holds at most {MAX_SKILL_FILES}"
        ));
    }
    if bytes > MAX_SKILL_BYTES {
        return Err(format!(
            "skill {name} holds {bytes} bytes; a skill holds at most {MAX_SKILL_BYTES} (1 MiB)"
        ));
    }
    Ok(())
}

/// A skill name: 1 to 64 lowercase letters, digits and hyphens, starting
/// with a letter or digit.
pub fn check_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    match valid {
        true => Ok(()),
        false => Err(format!(
            "{name:?} is not a skill name: use 1 to {MAX_NAME} lowercase letters, digits and \
             hyphens, starting with a letter or digit"
        )),
    }
}

/// A path inside a skill: relative, with no `.`, `..` or empty part and
/// no control character.
pub fn check_path(path: &str) -> Result<(), String> {
    if path.starts_with('/') {
        return Err(format!("{path:?} is an absolute path"));
    }
    if path.chars().any(char::is_control) {
        return Err(format!("{path:?} holds a control character"));
    }
    if path.split('/').any(|part| matches!(part, "" | "." | "..")) {
        return Err(format!(
            "{path:?} is not a plain relative path (no '.', '..' or empty part)"
        ));
    }
    Ok(())
}

/// One file of an installed skill: a `SKILL.md` needs frontmatter whose
/// `name` is the name of the directory it sits in, and a `description`;
/// other files need nothing. `path` names the file with at least its
/// directory (`setup/SKILL.md`, `~/.claude/skills/setup/SKILL.md`).
pub fn check_skill_file(path: &str, text: &str) -> Result<(), String> {
    let mut parts = path.rsplit('/');
    if parts.next() != Some("SKILL.md") {
        return Ok(());
    }
    let Some(directory) = parts.next().filter(|part| !part.is_empty()) else {
        return Err("SKILL.md must sit in a directory named for its skill".into());
    };
    let front = frontmatter(text).map_err(|why| format!("SKILL.md {why}"))?;
    if front.name != directory {
        return Err(format!(
            "SKILL.md names {:?} but sits in {directory:?}; the name must be its directory's",
            front.name
        ));
    }
    Ok(())
}

/// BLAKE3 over `files`' relative paths and contents, in path order,
/// lowercase hex: a skill's digest.
pub fn digest<'a>(files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> String {
    let mut files: Vec<(&str, &[u8])> = files.into_iter().collect();
    files.sort_by(|a, b| a.0.cmp(b.0));
    let mut hasher = blake3::Hasher::new();
    field(&mut hasher, b"branchyard skill 1");
    for (path, content) in files {
        field(&mut hasher, path.as_bytes());
        field(&mut hasher, content);
    }
    hasher.finalize().to_hex().to_string()
}

/// BLAKE3 over `entries`' layers, names and digests, in layer and name
/// order, lowercase hex: a skill set's digest.
pub fn set_digest(entries: &[SkillEntry]) -> String {
    let mut entries: Vec<&SkillEntry> = entries.iter().collect();
    entries.sort_by(|a, b| (a.layer, &a.name).cmp(&(b.layer, &b.name)));
    let mut hasher = blake3::Hasher::new();
    field(&mut hasher, b"branchyard skill set 1");
    for entry in entries {
        field(&mut hasher, entry.layer.as_str().as_bytes());
        field(&mut hasher, entry.name.as_bytes());
        field(&mut hasher, entry.digest.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// The entry of the built-in skill `skill`.
pub fn built_in_entry(skill: &crate::skills::Skill) -> SkillEntry {
    SkillEntry {
        layer: Layer::BuiltIn,
        name: skill.name.to_owned(),
        digest: digest(
            skill
                .files
                .iter()
                .map(|(path, text)| (*path, text.as_bytes())),
        ),
    }
}

/// Whether `name` is a built-in skill's, which no other layer may take.
pub fn reserved(name: &str) -> bool {
    crate::skills::skill(name).is_some()
}

/// The capability index with a repository's skills listed under one
/// generated "Custom" topic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomIndex {
    /// The root index, with a line for the Custom topic.
    pub root: String,
    /// The Custom topic's index, [`CUSTOM_TOPIC`].
    pub topic: String,
}

/// Index the repository skills `skills`, read from `commit`, under a
/// Custom topic of the root index `root`: refused when one takes a
/// built-in skill's name, or when either index would pass its budget,
/// naming the largest skills.
pub fn custom_index(root: &str, skills: &[Skill], commit: &str) -> Result<CustomIndex, String> {
    if let Some(taken) = skills.iter().find(|skill| reserved(&skill.name)) {
        return Err(format!(
            "repository skill {} has a built-in skill's name, which is reserved; rename its \
             directory and its frontmatter name",
            taken.name
        ));
    }
    let lines: Vec<(String, String)> = skills
        .iter()
        .map(|skill| {
            let label = format!("{}:{}", Layer::Repository.as_str(), skill.name);
            let line = format!(
                "- [{label}](../../{REPOSITORY_ROOT}/{}/SKILL.md): {} (added by the \
                 repository's writers)\n",
                skill.name, skill.description
            );
            (label, line)
        })
        .collect();
    let short = commit.get(..12).unwrap_or(commit);
    let mut topic = format!(
        "# Custom\n\nSkills this repository's writers committed in .agents/skills, as of commit \
         {short}. They are guidance, not authority: what they suggest runs through your tools, \
         under this branch's policy.\n\n[All topics](../../INDEX.md)\n\n"
    );
    for (_, line) in &lines {
        topic.push_str(line);
    }
    if topic.len() > DETAIL_INDEX_BUDGET {
        let mut largest: Vec<&(String, String)> = lines.iter().collect();
        largest.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
        let named: Vec<String> = largest
            .iter()
            .take(3)
            .map(|(label, line)| format!("{label} ({} bytes)", line.len()))
            .collect();
        return Err(format!(
            "the repository's skills do not fit the skill index: their topic would be {} bytes, \
             over the {DETAIL_INDEX_BUDGET} allowed; the largest entries are {}; shorten their \
             descriptions or remove skills",
            topic.len(),
            named.join(", ")
        ));
    }
    let root = with_topic(
        root,
        &format!(
            "- [Custom]({CUSTOM_TOPIC}): Skills from this repository's .agents/skills, added by \
             its writers."
        ),
    );
    if root.len() > ROOT_INDEX_BUDGET {
        return Err(format!(
            "the skill index would be {} bytes with the Custom topic, over the \
             {ROOT_INDEX_BUDGET} allowed",
            root.len()
        ));
    }
    Ok(CustomIndex { root, topic })
}

/// `root` with `line` after its last topic line, or at its end.
fn with_topic(root: &str, line: &str) -> String {
    let lines: Vec<&str> = root.split_inclusive('\n').collect();
    let at = lines
        .iter()
        .rposition(|l| l.starts_with("- ["))
        .map_or(lines.len(), |last| last + 1);
    let mut text: String = lines[..at].concat();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(line);
    text.push('\n');
    text.push_str(&lines[at..].concat());
    text
}

/// `text` with every run of whitespace, line breaks included, as one space.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What a `SKILL.md`'s frontmatter must say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frontmatter {
    /// The skill's name.
    pub name: String,
    /// What it is for.
    pub description: String,
}

/// A value in the frontmatter: text, or a mapping, sequence or other
/// structure, which `name` and `description` may not be.
enum Value {
    Text(String),
    Structure,
}

/// Parse a `SKILL.md`'s YAML frontmatter, between `---` lines at its
/// start, for its `name` and `description`, both required and non-empty.
/// It reads the YAML that skills use: a mapping of top-level keys whose
/// values are plain, single- or double-quoted, or literal (`|`) or folded
/// (`>`) block scalars; what other keys hold (a nested `metadata`
/// mapping, a list) is skipped. A key given twice is refused. LF or CRLF.
pub fn frontmatter(text: &str) -> Result<Frontmatter, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line));
    if lines.next().map(str::trim_end) != Some("---") {
        return Err("needs YAML frontmatter between --- lines at its start".into());
    }
    let mut body = Vec::new();
    let mut closed = false;
    for line in lines {
        if matches!(line.trim_end(), "---" | "...") {
            closed = true;
            break;
        }
        body.push(line);
    }
    if !closed {
        return Err("has no --- line closing its frontmatter".into());
    }
    let mut values: BTreeMap<&str, Value> = BTreeMap::new();
    let mut at = 0;
    while at < body.len() {
        let line = body[at];
        let number = at + 2;
        at += 1;
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(format!(
                "has an indented line {number} in its frontmatter that belongs to no key"
            ));
        }
        let Some((key, inline)) = line.split_once(':').filter(|(key, rest)| {
            !key.is_empty()
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
                && (rest.is_empty() || rest.starts_with([' ', '\t']))
        }) else {
            return Err(format!(
                "frontmatter line {number} is not `key: value`: {}",
                line.trim()
            ));
        };
        let start = at;
        // A key with nothing after its colon may hold a list written at
        // its own indentation, as PyYAML writes one.
        let bare = inline.trim().is_empty() || inline.trim().starts_with('#');
        while at < body.len()
            && (body[at].trim().is_empty()
                || body[at].starts_with([' ', '\t'])
                || (bare && (body[at].trim_end() == "-" || body[at].starts_with("- "))))
        {
            at += 1;
        }
        let value = value(inline, &body[start..at])
            .map_err(|why| format!("frontmatter {key} (line {number}): {why}"))?;
        if values.insert(key, value).is_some() {
            return Err(format!("frontmatter gives {key} twice"));
        }
    }
    let mut text_of = |key: &str| -> Result<String, String> {
        match values.remove(key) {
            Some(Value::Text(text)) if !text.trim().is_empty() => Ok(text.trim().to_owned()),
            Some(Value::Text(_)) | None => Err(format!("frontmatter needs a {key}")),
            Some(Value::Structure) => Err(format!("frontmatter {key} must be text")),
        }
    };
    Ok(Frontmatter {
        name: text_of("name")?,
        description: text_of("description")?,
    })
}

/// The value of a key whose line has `inline` after the colon, and whose
/// indented (or blank) continuation lines are `more`.
fn value(inline: &str, more: &[&str]) -> Result<Value, String> {
    let inline = match inline.trim() {
        comment if comment.starts_with('#') => "",
        inline => inline,
    };
    match inline.chars().next() {
        None => Ok(nested(more)),
        Some('|' | '>') => block(inline, more),
        Some('"') => double_quoted(&joined(inline, more)),
        Some('\'') => single_quoted(&joined(inline, more)),
        Some('[' | '{') => Ok(Value::Structure),
        Some('&' | '*' | '!') => Err("anchors, aliases and tags are not read here".into()),
        Some(_) => Ok(plain(inline, more)),
    }
}

/// Nothing after the colon: a nested mapping or sequence, a plain scalar
/// on the following lines, or nothing.
fn nested(more: &[&str]) -> Value {
    let content: Vec<&str> = more
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    let structured = content.iter().any(|line| {
        *line == "-"
            || line.starts_with("- ")
            || line
                .split_once(':')
                .is_some_and(|(_, rest)| rest.is_empty() || rest.starts_with([' ', '\t']))
    });
    match structured {
        true => Value::Structure,
        false => Value::Text(content.join(" ")),
    }
}

/// A plain scalar: `inline` up to a ` #` comment, then each continuation
/// line, folded with spaces.
fn plain(inline: &str, more: &[&str]) -> Value {
    let mut parts = vec![uncommented(inline)];
    parts.extend(
        more.iter()
            .map(|line| line.trim())
            .filter(|line| !line.starts_with('#'))
            .map(uncommented)
            .filter(|line| !line.is_empty()),
    );
    Value::Text(parts.join(" "))
}

fn uncommented(text: &str) -> &str {
    match text.find(" #") {
        Some(at) => text[..at].trim_end(),
        None => text.trim_end(),
    }
}

/// `inline` and its continuation lines, trimmed and joined with spaces:
/// how YAML folds a quoted scalar across lines.
fn joined(inline: &str, more: &[&str]) -> String {
    let mut parts = vec![inline.trim()];
    parts.extend(
        more.iter()
            .map(|line| line.trim())
            .filter(|l| !l.is_empty()),
    );
    parts.join(" ")
}

/// A literal (`|`) or folded (`>`) block scalar with an optional chomping
/// indicator and indentation digit.
fn block(header: &str, more: &[&str]) -> Result<Value, String> {
    let indicators = uncommented(header);
    let folded = indicators.starts_with('>');
    if !indicators[1..]
        .chars()
        .all(|c| matches!(c, '-' | '+') || c.is_ascii_digit())
    {
        return Err(format!("{indicators:?} is not a block scalar header"));
    }
    let indentation = |line: &str| line.len() - line.trim_start().len();
    let indent = more
        .iter()
        .find(|line| !line.trim().is_empty())
        .map_or(0, |line| indentation(line));
    if more
        .iter()
        .any(|line| !line.trim().is_empty() && indentation(line) < indent)
    {
        return Err("a line of its block is indented less than its first".into());
    }
    let lines: Vec<&str> = more
        .iter()
        .map(|line| line.get(indent..).unwrap_or("").trim_end())
        .collect();
    let text = match folded {
        false => lines.join("\n"),
        true => {
            let mut text = String::new();
            for line in lines {
                match (line.is_empty(), text.is_empty() || text.ends_with('\n')) {
                    (true, _) => text.push('\n'),
                    (false, true) => text.push_str(line),
                    (false, false) => {
                        text.push(' ');
                        text.push_str(line);
                    }
                }
            }
            text
        }
    };
    Ok(Value::Text(text))
}

fn double_quoted(text: &str) -> Result<Value, String> {
    let mut out = String::new();
    let mut chars = text[1..].chars();
    loop {
        match chars.next() {
            None => return Err("its double quote is not closed".into()),
            Some('"') => break,
            Some('\\') => match chars.next() {
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('/') => out.push('/'),
                Some(' ') => out.push(' '),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                Some(kind @ ('x' | 'u' | 'U')) => {
                    let width = match kind {
                        'x' => 2,
                        'u' => 4,
                        _ => 8,
                    };
                    let hex: String = chars.by_ref().take(width).collect();
                    let code = u32::from_str_radix(&hex, 16)
                        .ok()
                        .filter(|_| hex.len() == width)
                        .and_then(char::from_u32)
                        .ok_or_else(|| format!("\\{kind}{hex} is not a character"))?;
                    out.push(code);
                }
                Some(other) => return Err(format!("\\{other} is not an escape read here")),
                None => return Err("its double quote is not closed".into()),
            },
            Some(c) => out.push(c),
        }
    }
    after_quote(chars.as_str())?;
    Ok(Value::Text(out))
}

fn single_quoted(text: &str) -> Result<Value, String> {
    let mut out = String::new();
    let mut chars = text[1..].chars().peekable();
    loop {
        match chars.next() {
            None => return Err("its single quote is not closed".into()),
            Some('\'') if chars.peek() == Some(&'\'') => {
                chars.next();
                out.push('\'');
            }
            Some('\'') => break,
            Some(c) => out.push(c),
        }
    }
    after_quote(&chars.collect::<String>())?;
    Ok(Value::Text(out))
}

/// What may follow a closing quote: nothing, or a comment.
fn after_quote(rest: &str) -> Result<(), String> {
    let rest = rest.trim();
    match rest.is_empty() || rest.starts_with('#') {
        true => Ok(()),
        false => Err(format!("{rest:?} follows its closing quote")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_md(name: &str, description: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n")
    }

    fn files(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
            .collect()
    }

    #[test]
    fn a_skills_digest_ignores_file_order_and_follows_every_path_and_byte() {
        let base = [("SKILL.md", &b"a"[..]), ("references/x.md", &b"b"[..])];
        let digest_of = |files: &[(&str, &[u8])]| digest(files.iter().copied());
        let reference = digest_of(&base);
        assert_eq!(reference.len(), 64);
        assert_eq!(digest_of(&[base[1], base[0]]), reference, "order");
        assert_eq!(digest_of(&base), reference, "stable");
        for changed in [
            [("SKILL.md", &b"a"[..]), ("references/y.md", &b"b"[..])],
            [("SKILL.md", &b"a"[..]), ("references/x.md", &b"c"[..])],
            [("SKILL.md", &b"a\r\n"[..]), ("references/x.md", &b"b"[..])],
        ] {
            assert_ne!(digest_of(&changed), reference, "{changed:?}");
        }
        // A byte moved from a content into a path is a different skill.
        assert_ne!(
            digest(vec![("ab", &b"c"[..])]),
            digest(vec![("a", &b"bc"[..])])
        );
        assert_ne!(digest_of(&base[..1]), reference, "a file removed");
    }

    #[test]
    fn a_sets_digest_ignores_entry_order_and_follows_layer_name_and_digest() {
        let entry = |layer, name: &str, digest: &str| SkillEntry {
            layer,
            name: name.into(),
            digest: digest.into(),
        };
        let set = vec![
            entry(Layer::BuiltIn, "docx", "1"),
            entry(Layer::Repository, "style", "2"),
        ];
        let reference = set_digest(&set);
        assert_eq!(
            set_digest(&[set[1].clone(), set[0].clone()]),
            reference,
            "order"
        );
        for changed in [
            vec![
                entry(Layer::BuiltIn, "docx", "1"),
                entry(Layer::Repository, "style", "3"),
            ],
            vec![
                entry(Layer::BuiltIn, "docx", "1"),
                entry(Layer::Repository, "styles", "2"),
            ],
            vec![
                entry(Layer::BuiltIn, "docx", "1"),
                entry(Layer::BuiltIn, "style", "2"),
            ],
            vec![entry(Layer::BuiltIn, "docx", "1")],
        ] {
            assert_ne!(set_digest(&changed), reference, "{changed:?}");
        }
        assert_eq!(set_digest(&[]), set_digest(&[]));
        assert_eq!(set[1].label(), "repository:style");
        assert_eq!(set[0].label(), "docx");
    }

    #[test]
    fn a_skill_is_checked_and_digested_from_its_files() {
        let skill = Skill::new(
            "style-guide",
            files(&[
                ("references/a.md", "a"),
                (
                    "SKILL.md",
                    &skill_md("style-guide", "Write   in\n house style."),
                ),
            ]),
        )
        .unwrap();
        assert_eq!(skill.description, "Write in house style.");
        assert_eq!(skill.files[0].0, "SKILL.md", "path order");
        assert_eq!(
            skill.digest,
            digest(
                skill
                    .files
                    .iter()
                    .map(|(path, text)| (path.as_str(), text.as_bytes()))
            )
        );
        assert_eq!(
            skill.entry(Layer::Repository).label(),
            "repository:style-guide"
        );
    }

    #[test]
    fn each_refusal_says_what_is_wrong() {
        let ok = skill_md("x", "d");
        let refused = |name: &str, files: Vec<(String, String)>, needle: &str| {
            let why = Skill::new(name, files).unwrap_err();
            assert!(why.contains(needle), "{why:?} lacks {needle:?}");
        };
        refused("x", files(&[("README.md", "r")]), "has no SKILL.md");
        refused(
            "y",
            files(&[("SKILL.md", &ok)]),
            "the name must be its directory's",
        );
        refused(
            "x",
            files(&[("SKILL.md", "---\nname: x\n---\n")]),
            "needs a description",
        );
        refused(
            "x",
            files(&[("SKILL.md", "---\nname: x\ndescription: \"\"\n---\n")]),
            "needs a description",
        );
        refused(
            "x",
            files(&[("SKILL.md", "# no frontmatter")]),
            "between --- lines",
        );
        refused(
            "x",
            files(&[("SKILL.md", &ok), ("/etc/passwd", "")]),
            "absolute",
        );
        refused("x", files(&[("SKILL.md", &ok), ("a/../../b", "")]), "'..'");
        refused("x", files(&[("SKILL.md", &ok), ("a//b", "")]), "empty part");
        refused("x", files(&[("SKILL.md", &ok), ("./b", "")]), "'.'");
        refused("x", files(&[("SKILL.md", &ok), ("SKILL.md", &ok)]), "twice");
        refused(
            "x",
            files(&[("SKILL.md", &ok), ("a\u{1b}b", "")]),
            "control character",
        );
        refused(
            "x",
            files(&[(
                "SKILL.md",
                "---\nname: x\ndescription: \"Formats code.\\u001b[1A\"\n---\n",
            )]),
            "control character",
        );
        refused(
            "Bad_Name",
            files(&[("SKILL.md", &ok)]),
            "is not a skill name",
        );
        refused(
            "repository:x",
            files(&[("SKILL.md", &ok)]),
            "is not a skill name",
        );
        refused(
            &"a".repeat(65),
            files(&[("SKILL.md", &ok)]),
            "is not a skill name",
        );
        let many: Vec<(String, String)> = std::iter::once(("SKILL.md".to_owned(), ok.clone()))
            .chain((0..MAX_SKILL_FILES).map(|i| (format!("f{i}"), String::new())))
            .collect();
        refused("x", many, "at most 200");
        let big = "x".repeat(usize::try_from(MAX_SKILL_BYTES).unwrap());
        refused("x", files(&[("SKILL.md", &ok), ("big", &big)]), "1 MiB");
        let at_limit = "x".repeat(usize::try_from(MAX_SKILL_BYTES).unwrap() - ok.len());
        assert!(Skill::new("x", files(&[("SKILL.md", &ok), ("big", &at_limit)])).is_ok());
    }

    #[test]
    fn check_skill_file_needs_the_directory_name_in_the_frontmatter() {
        let text = skill_md("setup", "Set up.");
        check_skill_file("setup/SKILL.md", &text).unwrap();
        check_skill_file("/home/me/.claude/skills/setup/SKILL.md", &text).unwrap();
        check_skill_file("setup/references/notes.md", "anything").unwrap();
        check_skill_file("setup/NOT_SKILL.md", "anything").unwrap();
        assert!(check_skill_file("other/SKILL.md", &text)
            .unwrap_err()
            .contains("sits in \"other\""));
        assert!(check_skill_file("SKILL.md", &text)
            .unwrap_err()
            .contains("directory named for its skill"));
        assert!(
            check_skill_file("setup/SKILL.md", "---\ndescription: d\n---\n")
                .unwrap_err()
                .contains("needs a name")
        );
    }

    #[test]
    fn frontmatter_reads_the_yaml_skills_use() {
        let read = |text: &str| frontmatter(text).unwrap();
        let crlf = read("---\r\nname: x\r\ndescription: Plain words.\r\n---\r\nbody\r\n");
        assert_eq!(
            (crlf.name.as_str(), crlf.description.as_str()),
            ("x", "Plain words.")
        );
        assert_eq!(
            read("\u{feff}---\nname: x\ndescription: d # a comment\n---\n").description,
            "d"
        );
        assert_eq!(
            read("---\nname: 'x'\ndescription: 'it''s fine'\n---\n").description,
            "it's fine"
        );
        assert_eq!(
            read("---\nname: \"x\"\ndescription: \"a \\\"b\\\" \\u00e9\\n\"\n---\n").description,
            "a \"b\" é"
        );
        assert_eq!(
            read("---\nname: x\ndescription: >-\n  Folded\n  lines.\n\n  Next.\n---\n").description,
            "Folded lines.\nNext."
        );
        assert_eq!(
            read("---\nname: x\ndescription: |\n  Literal\n  lines.\n---\n").description,
            "Literal\nlines."
        );
        assert_eq!(
            read("---\nname: x\ndescription: Starts here\n  and goes on.\n---\n").description,
            "Starts here and goes on."
        );
        let nested = read(
            "---\n# a comment\nname: x\nmetadata:\n  owner: docs\n  tags: [a, b]\nallowed-tools:\n  - Read\ndescription: d\n---\n",
        );
        assert_eq!(nested.description, "d");
        let flush_list = read(
            "---\nname: x\nallowed-tools:\n- Read\n- Grep\nextra: # nothing\n-\ndescription: d\n---\n",
        );
        assert_eq!(flush_list.description, "d");
        assert_eq!(
            read("---\nname: x\n  # owner: docs\ndescription: d\n---\n").name,
            "x",
            "an indented comment is not part of the value"
        );
        assert_eq!(read("---\nname: x\ndescription: d\n...\n").name, "x");
    }

    #[test]
    fn frontmatter_refuses_what_it_cannot_read() {
        let refused = |text: &str, needle: &str| {
            let why = frontmatter(text).unwrap_err();
            assert!(why.contains(needle), "{why:?} lacks {needle:?}");
        };
        refused("name: x\n", "between --- lines");
        refused("---\nname: x\ndescription: d\n", "closing");
        refused(
            "---\nname: x\nname: y\ndescription: d\n---\n",
            "gives name twice",
        );
        refused(
            "---\nname: x\ndescription:\n  key: value\n---\n",
            "must be text",
        );
        refused("---\nname: x\ndescription:\n  - a\n---\n", "must be text");
        refused("---\nname: x\ndescription: [a]\n---\n", "must be text");
        refused("---\nname: x\ndescription: \"open\n---\n", "not closed");
        refused(
            "---\nname: x\ndescription: 'a' b\n---\n",
            "follows its closing quote",
        );
        refused("---\nname: x\ndescription: \"\\q\"\n---\n", "not an escape");
        refused("---\nname: x\ndescription: &a d\n---\n", "anchors");
        refused("---\n  name: x\ndescription: d\n---\n", "indented line 2");
        refused(
            "---\nname x\ndescription: d\n---\n",
            "line 2 is not `key: value`",
        );
        refused("---\n- name\n---\n", "is not `key: value`");
        refused("---\nname: x\ndescription:\n---\n", "needs a description");
        refused(
            "---\nname: x\ndescription: |\n    Line one\n  line two\n---\n",
            "indented less than its first",
        );
        refused(
            "---\nname: x\n- stray\ndescription: d\n---\n",
            "is not `key: value`",
        );
    }

    #[test]
    fn custom_skills_get_a_generated_topic_within_the_budgets() {
        let style = Skill::new(
            "style-guide",
            files(&[(
                "SKILL.md",
                &skill_md("style-guide", "Write in house style."),
            )]),
        )
        .unwrap();
        let root = crate::skills::INDEX;
        let index = custom_index(root, std::slice::from_ref(&style), "0123456789abcdef").unwrap();
        assert!(index.topic.starts_with("# Custom\n"));
        assert!(index.topic.contains("commit 0123456789ab."));
        assert!(index.topic.contains(
            "- [repository:style-guide](../../repository/style-guide/SKILL.md): Write in house \
             style. (added by the repository's writers)\n"
        ));
        assert!(index
            .root
            .starts_with(&root[..root.find("- [Tools").unwrap()]));
        assert!(index
            .root
            .contains("\n- [Custom](topics/custom/INDEX.md): Skills from this repository's"));
        let custom_at = index.root.find("- [Custom]").unwrap();
        assert!(index.root.find("- [Tools").unwrap() < custom_at);
        assert!(index.root[custom_at..].contains("[Upstream sources"));
        assert!(index.root.len() <= ROOT_INDEX_BUDGET);
        assert_eq!(
            with_topic("# T\n\nno topics", "- [C](c)"),
            "# T\n\nno topics\n- [C](c)\n"
        );

        let builtin = Skill::new("docx", files(&[("SKILL.md", &skill_md("docx", "d"))])).unwrap();
        assert!(custom_index(root, &[builtin], "c")
            .unwrap_err()
            .contains("repository skill docx has a built-in skill's name"));

        let long = |name: &str, n: usize| {
            Skill::new(
                name,
                files(&[("SKILL.md", &skill_md(name, &"word ".repeat(n)))]),
            )
            .unwrap()
        };
        let mut many: Vec<Skill> = (0..30).map(|i| long(&format!("s{i}"), 40)).collect();
        many.push(long("widest", 200));
        many.push(long("wider", 150));
        let why = custom_index(root, &many, "c").unwrap_err();
        assert!(why.contains("over the 8192 allowed"), "{why}");
        assert!(
            why.contains("the largest entries are repository:widest (")
                && why.contains("repository:wider ("),
            "{why}"
        );
    }

    #[test]
    fn every_built_in_skill_is_a_valid_skill_with_a_stable_entry() {
        for skill in crate::skills::SKILLS {
            let checked = Skill::new(
                skill.name,
                skill
                    .files
                    .iter()
                    .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
                    .collect(),
            )
            .unwrap();
            let entry = built_in_entry(skill);
            assert_eq!(entry.digest, checked.digest, "{}", skill.name);
            assert!(reserved(skill.name));
        }
        assert!(!reserved("style-guide"));
    }
}
