//! What a tool call acted on, in a form safe to keep in the event log.
//!
//! A [`ToolTarget`] is a bounded, privacy-safe summary of a tool call's
//! input: the path a file tool read or edited, the command a shell tool
//! ran, or the server and tool of an MCP call. It is never a file's
//! content and never the call's full arguments.
//!
//! - A path inside the session's working directory is relative to it. A
//!   path in the mounted tool tree (`/tools/...`, or anything under
//!   `.branchyard/tools/`) is written `tools/...`. Any other absolute path
//!   is reduced to its last component, so the log does not carry a host's
//!   directory layout.
//! - A command is collapsed to one line, has secrets scrubbed (values of
//!   `NAME=secret` assignments and of `--token`-style flags, bearer
//!   tokens, URL credentials and well-known token shapes), and is cut at
//!   [`COMMAND_MAX`] characters.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The most characters of a command a target keeps.
pub const COMMAND_MAX: usize = 120;
/// The most characters of a path, skill name or tool name a target keeps.
pub const NAME_MAX: usize = 160;
/// What replaces a secret.
pub const REDACTED: &str = "[redacted]";

/// What kind of thing a [`ToolTarget`] names.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    /// A file the tool read or changed.
    File,
    /// A shell command.
    Command,
    /// An MCP or dynamic tool, as `server.tool` (or the tool alone).
    Mcp,
    /// A skill the harness's own skill tool opened.
    Skill,
}

/// What a tool call acted on.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTarget {
    /// What `value` names.
    pub kind: TargetKind,
    /// The path, command or tool name, bounded and scrubbed as the module
    /// documents.
    pub value: String,
}

impl ToolTarget {
    /// A file, with `raw` made relative to `cwd` or reduced as documented.
    pub fn file(raw: &str, cwd: &str) -> Option<ToolTarget> {
        let value = path(raw, cwd)?;
        Some(ToolTarget {
            kind: TargetKind::File,
            value,
        })
    }

    /// A shell command, scrubbed and bounded; its absolute paths are
    /// reduced as [`path`] does with no working directory.
    pub fn command(raw: &str) -> Option<ToolTarget> {
        ToolTarget::command_in(raw, "")
    }

    /// A shell command run in `cwd`, scrubbed and bounded; a word that is an
    /// absolute path is reduced as [`path`] does, so the command's first
    /// words fit in the bound and the log carries no host layout.
    pub fn command_in(raw: &str, cwd: &str) -> Option<ToolTarget> {
        let value = command(raw, cwd)?;
        Some(ToolTarget {
            kind: TargetKind::Command,
            value,
        })
    }

    /// An MCP or dynamic tool call.
    pub fn mcp(server: Option<&str>, tool: &str) -> Option<ToolTarget> {
        let tool = tool.trim();
        if tool.is_empty() {
            return None;
        }
        let value = match server.map(str::trim).filter(|s| !s.is_empty()) {
            Some(server) => format!("{server}.{tool}"),
            None => tool.to_owned(),
        };
        Some(ToolTarget {
            kind: TargetKind::Mcp,
            value: cut(&value, NAME_MAX),
        })
    }

    /// A skill opened by name.
    pub fn skill(name: &str) -> Option<ToolTarget> {
        let name = name.trim();
        (!name.is_empty()).then(|| ToolTarget {
            kind: TargetKind::Skill,
            value: cut(name, NAME_MAX),
        })
    }
}

/// Keys whose string value is a shell command.
const COMMAND_KEYS: [&str; 6] = [
    "command",
    "cmd",
    "commandline",
    "command_line",
    "script",
    "shell",
];
/// Keys whose string value is a path.
const PATH_KEYS: [&str; 7] = [
    "file_path",
    "filepath",
    "path",
    "notebook_path",
    "file",
    "filename",
    "uri",
];

/// The target of a tool call from its name and input object, for harnesses
/// that report both: a `mcp__server__tool` name is an MCP call; the native
/// skill tool names the skill; an input with a command is a command; one
/// with a path is a file. `None` when the input shows none of these.
pub fn from_input(tool: &str, input: &Value, cwd: &str) -> Option<ToolTarget> {
    if let Some(rest) = tool.strip_prefix("mcp__") {
        return match rest.split_once("__") {
            Some((server, name)) => ToolTarget::mcp(Some(server), name),
            None => ToolTarget::mcp(None, rest),
        };
    }
    let object = input.as_object()?;
    if tool.eq_ignore_ascii_case("skill") {
        let name = ["skill", "name"]
            .iter()
            .find_map(|key| object.get(*key).and_then(Value::as_str));
        if let Some(target) = name.and_then(ToolTarget::skill) {
            return Some(target);
        }
    }
    let string_at = |keys: &[&str]| {
        object.iter().find_map(|(key, value)| {
            let key = key.to_ascii_lowercase();
            match keys.contains(&key.as_str()) {
                true => value.as_str(),
                false => None,
            }
        })
    };
    if let Some(command) = string_at(&COMMAND_KEYS).and_then(|raw| ToolTarget::command_in(raw, cwd))
    {
        return Some(command);
    }
    let path = string_at(&PATH_KEYS).or_else(|| {
        // `paths` and `files` lists: the first is the target.
        ["paths", "files"]
            .iter()
            .find_map(|key| object.get(*key)?.as_array()?.first()?.as_str())
    })?;
    ToolTarget::file(path, cwd)
}

/// `raw` as a log-safe path: relative to `cwd`, `tools/...` for the tool
/// tree, otherwise the last component of an absolute path.
pub fn path(raw: &str, cwd: &str) -> Option<String> {
    let raw = raw.trim();
    let raw = raw.strip_prefix("file://").unwrap_or(raw);
    if raw.is_empty() || raw.chars().any(char::is_control) {
        return None;
    }
    let value = if let Some(rest) = after_marker(raw) {
        format!("tools/{rest}")
    } else if raw.starts_with('/') {
        let cwd = cwd.trim_end_matches('/');
        match raw
            .strip_prefix(cwd)
            .filter(|rest| !cwd.is_empty() && (rest.is_empty() || rest.starts_with('/')))
        {
            Some(rest) => match rest.trim_start_matches('/') {
                "" => ".".to_owned(),
                inside => inside.to_owned(),
            },
            None => raw
                .rsplit('/')
                .find(|part| !part.is_empty())
                .unwrap_or(raw)
                .to_owned(),
        }
    } else {
        raw.trim_start_matches("./").to_owned()
    };
    Some(cut(&value, NAME_MAX))
}

/// What follows the tool tree's directory in `raw`, when it names one.
fn after_marker(raw: &str) -> Option<&str> {
    if let Some(rest) = raw.strip_prefix("/tools/") {
        return Some(rest);
    }
    raw.split_once(".branchyard/tools/").map(|(_, rest)| rest)
}

/// `raw` as a one-line, scrubbed command of at most [`COMMAND_MAX`]
/// characters, with a plain shell wrapper (`bash -lc "..."`) removed and
/// each word that is an absolute path reduced as [`path`] reduces it.
pub fn command(raw: &str, cwd: &str) -> Option<String> {
    let unwrapped = unwrap_shell(raw.trim());
    let words: Vec<String> = unwrapped
        .split_whitespace()
        .map(|word| word.chars().filter(|c| !c.is_control()).collect())
        .collect();
    if words.is_empty() {
        return None;
    }
    let reduced: Vec<String> = scrub_words(&words)
        .iter()
        .map(|word| reduce_path_word(word, cwd))
        .collect();
    Some(cut(&reduced.join(" "), COMMAND_MAX))
}

/// `word` with its path, when it is one (`/a/b`, `'/a/b'`), reduced.
fn reduce_path_word(word: &str, cwd: &str) -> String {
    let quotes = |c: char| matches!(c, '\'' | '"' | ';' | '(' | ')');
    let core = word.trim_matches(quotes);
    if !core.starts_with('/') || core.len() < 2 {
        return word.to_owned();
    }
    let Some(reduced) = path(core, cwd) else {
        return word.to_owned();
    };
    let lead = &word[..word.len() - word.trim_start_matches(quotes).len()];
    let tail = &word[word.trim_end_matches(quotes).len()..];
    format!("{lead}{reduced}{tail}")
}

/// The script of `sh -c SCRIPT` and `bash -lc SCRIPT`, without the quotes
/// around it; anything else unchanged.
fn unwrap_shell(raw: &str) -> &str {
    let mut words = raw.splitn(3, char::is_whitespace);
    let (Some(shell), Some(flag), Some(rest)) = (words.next(), words.next(), words.next()) else {
        return raw;
    };
    let name = shell.rsplit('/').next().unwrap_or(shell);
    let shell_like = matches!(name, "sh" | "bash" | "zsh" | "dash");
    let runs_script = flag.starts_with('-') && flag.ends_with('c') && !flag.starts_with("--");
    if !(shell_like && runs_script) {
        return raw;
    }
    let rest = rest.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = rest
            .strip_prefix(quote)
            .and_then(|inner| inner.strip_suffix(quote))
        {
            return inner;
        }
    }
    rest
}

/// Name fragments that make a variable or flag hold a secret.
const SECRET_WORDS: [&str; 11] = [
    "key",
    "token",
    "secret",
    "password",
    "passwd",
    "pwd",
    "credential",
    "auth",
    "cookie",
    "session",
    "bearer",
];
/// Prefixes of well-known token shapes.
const TOKEN_PREFIXES: [&str; 12] = [
    "sk-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "xox",
    "glpat-",
    "AKIA",
    "AIza",
    "npm_",
];

fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    SECRET_WORDS.iter().any(|word| name.contains(word))
}

fn token_shaped(word: &str) -> bool {
    let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
    if TOKEN_PREFIXES
        .iter()
        .any(|prefix| word.starts_with(prefix) && word.len() >= prefix.len() + 8)
    {
        return true;
    }
    // A long run of token characters with a digit and a letter in it is a
    // credential far more often than a word.
    word.len() >= 32
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '/' | '='))
        && word.chars().any(|c| c.is_ascii_digit())
        && word.chars().any(|c| c.is_ascii_alphabetic())
        && !word.contains('/')
}

/// `word` with the password of a URL's credentials removed.
fn url_credentials(word: &str) -> Option<String> {
    let (scheme, rest) = word.split_once("://")?;
    let (authority, tail) = match rest.find('/') {
        Some(at) => rest.split_at(at),
        None => (rest, ""),
    };
    let (_, host) = authority.rsplit_once('@')?;
    Some(format!("{scheme}://{REDACTED}@{host}{tail}"))
}

fn scrub_words(words: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(words.len());
    let mut hide_next = false;
    for word in words {
        if hide_next {
            hide_next = false;
            out.push(REDACTED.to_owned());
            continue;
        }
        let lower = word.to_ascii_lowercase();
        if lower == "bearer" || lower == "basic" {
            out.push(word.clone());
            hide_next = true;
            continue;
        }
        if let Some(flag) = word.strip_prefix('-') {
            let flag = flag.trim_start_matches('-');
            if let Some((name, _)) = flag.split_once('=') {
                if secret_name(name) {
                    out.push(format!(
                        "{}={REDACTED}",
                        word.split('=').next().unwrap_or_default()
                    ));
                    continue;
                }
            } else if secret_name(flag) {
                out.push(word.clone());
                hide_next = true;
                continue;
            }
        }
        if let Some((name, value)) = word.split_once('=') {
            let is_assignment = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if is_assignment && !value.is_empty() && secret_name(name) {
                out.push(format!("{name}={REDACTED}"));
                continue;
            }
        }
        if let Some(clean) = url_credentials(word) {
            out.push(clean);
        } else if token_shaped(word) {
            out.push(REDACTED.to_owned());
        } else if lower.starts_with("authorization:") {
            out.push(word.clone());
            hide_next = true;
        } else {
            out.push(word.clone());
        }
    }
    out
}

/// `text` cut to `max` characters, with `...` marking a cut.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{}...", kept.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_path_inside_the_working_directory_is_relative() {
        assert_eq!(
            path("/workspace/src/a.rs", "/workspace").as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(
            path("./notes.txt", "/workspace").as_deref(),
            Some("notes.txt")
        );
        assert_eq!(path("/workspace", "/workspace").as_deref(), Some("."));
        assert_eq!(
            path("/workspace2/x.txt", "/workspace").as_deref(),
            Some("x.txt")
        );
    }

    #[test]
    fn a_tool_tree_path_keeps_its_place_in_the_tree() {
        assert_eq!(
            path("/tools/skills/docx/SKILL.md", "/workspace").as_deref(),
            Some("tools/skills/docx/SKILL.md")
        );
        assert_eq!(
            path("/home/me/.branchyard/tools/INDEX.md", "/workspace").as_deref(),
            Some("tools/INDEX.md")
        );
    }

    #[test]
    fn an_absolute_path_elsewhere_is_reduced_to_its_basename() {
        assert_eq!(
            path("/etc/ssh/id_rsa", "/workspace").as_deref(),
            Some("id_rsa")
        );
        assert_eq!(
            path("file:///Users/me/secret.txt", "/workspace").as_deref(),
            Some("secret.txt")
        );
        assert_eq!(path("", "/workspace"), None);
    }

    #[test]
    fn a_command_is_one_line_scrubbed_and_bounded() {
        assert_eq!(
            command("API_TOKEN=abc123 curl -H 'Authorization: Bearer abc.def' https://u:pw@host/x --password hunter2", "").as_deref(),
            Some("API_TOKEN=[redacted] curl -H 'Authorization: Bearer [redacted] https://[redacted]@host/x --password [redacted]")
        );
        assert_eq!(
            command("git push\n  --token=zzz origin ghp_abcdefghijklmnop", "").as_deref(),
            Some("git push --token=[redacted] origin [redacted]")
        );
        let long = format!("echo {}", "word ".repeat(60));
        let cut = command(&long, "").unwrap_or_default();
        assert!(cut.starts_with("echo word"));
        assert!(cut.ends_with("..."));
        assert!(cut.chars().count() <= COMMAND_MAX + 3);
        assert_eq!(command("   ", ""), None);
    }

    #[test]
    fn absolute_paths_in_a_command_are_reduced_so_its_first_words_survive_the_bound() {
        let deep = "/srv/run-1/repo/.branchyard/capabilities/ab12/.branchyard/tools/bin/tools";
        assert_eq!(
            command(
                &format!("{deep} search word document | head -3"),
                "/srv/run-1/repo"
            )
            .as_deref(),
            Some("tools/bin/tools search word document | head -3")
        );
        assert_eq!(
            command(
                "python3 /workspace/scripts/run.py --out '/home/me/out.txt'",
                "/workspace"
            )
            .as_deref(),
            Some("python3 scripts/run.py --out 'out.txt'")
        );
        assert_eq!(
            command("cat /tools/skills/docx/SKILL.md;", "/workspace").as_deref(),
            Some("cat tools/skills/docx/SKILL.md;")
        );
        assert_eq!(
            command("cd / && ls", "/workspace").as_deref(),
            Some("cd / && ls")
        );
    }

    #[test]
    fn a_shell_wrapper_is_removed_before_the_command_is_cut() {
        assert_eq!(
            command(r#"/bin/bash -lc "cat rec.txt && wc -c < rec.txt""#, "").as_deref(),
            Some("cat rec.txt && wc -c < rec.txt")
        );
        assert_eq!(
            command("bash script.sh", "").as_deref(),
            Some("bash script.sh")
        );
    }

    #[test]
    fn input_objects_become_the_right_kind_of_target() {
        let cwd = "/workspace";
        assert_eq!(
            from_input(
                "Bash",
                &json!({"command": "ls -la", "description": "x"}),
                cwd
            ),
            ToolTarget::command("ls -la")
        );
        assert_eq!(
            from_input("Read", &json!({"file_path": "/workspace/a.txt"}), cwd),
            ToolTarget::file("a.txt", cwd)
        );
        assert_eq!(
            from_input("Skill", &json!({"skill": "docx"}), cwd),
            ToolTarget::skill("docx")
        );
        assert_eq!(
            from_input("mcp__github__list_issues", &json!({"repo": "a/b"}), cwd),
            ToolTarget::mcp(Some("github"), "list_issues")
        );
        assert_eq!(
            from_input("run_command", &json!({"CommandLine": "pwd"}), cwd),
            ToolTarget::command("pwd")
        );
        assert_eq!(from_input("Task", &json!({"prompt": "do it"}), cwd), None);
        assert_eq!(from_input("Read", &Value::Null, cwd), None);
    }

    #[test]
    fn a_file_content_key_is_never_a_target() {
        let input = json!({"content": "secret body", "text": "more"});
        assert_eq!(from_input("Write", &input, "/workspace"), None);
    }
}
