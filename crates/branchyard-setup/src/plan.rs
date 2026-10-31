//! What a finished interview proposes: files, each with its diff against
//! what exists and the verdict of the real loader for its kind, and the
//! commands to run next. Nothing here writes; the front-end applies a plan
//! after the person agrees.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What a file is, which decides its validator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// `branchyard.toml` or the user `config.toml`: [`crate::config::parse`].
    ProjectConfig,
    /// A server's JSON configuration: `branchyard-server`'s loader.
    ServerConfig,
    /// A rig spec: `by rig check`'s parser and planner.
    Rig,
    /// A Docker Compose file.
    Compose,
    /// A bearer token or other generated secret: never shown.
    Secret,
    /// A skill's `SKILL.md` or reference file.
    Skill,
    /// A harness's own settings, hooks or extension file, installed by
    /// `by init plugin`: parsed as JSON or TOML by its extension (YAML is
    /// not parsed here).
    HarnessConfig,
    /// Anything else (`.gitignore`).
    Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileAction {
    Create,
    Update,
    /// Already exactly this; not rewritten.
    Unchanged,
    /// A secret file that exists: kept, never read or rewritten.
    Keep,
}

/// A loader's verdict on a file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Validation {
    /// Which loader ran, such as `branchyard-server config`.
    pub validator: String,
    pub ok: bool,
    /// True when no loader could run here (the verdict is then `ok`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
    /// Errors when not `ok`, else warnings and notes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<String>,
}

impl Validation {
    pub fn ok(validator: &str) -> Validation {
        Validation {
            validator: validator.into(),
            ok: true,
            ..Validation::default()
        }
    }

    pub fn failed(validator: &str, message: impl Into<String>) -> Validation {
        Validation {
            validator: validator.into(),
            ok: false,
            skipped: false,
            messages: vec![message.into()],
        }
    }

    pub fn skipped(validator: &str, why: impl Into<String>) -> Validation {
        Validation {
            validator: validator.into(),
            ok: true,
            skipped: true,
            messages: vec![why.into()],
        }
    }
}

/// One file of a plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PlannedFile {
    /// Relative to the project root, or absolute.
    pub path: String,
    pub kind: ArtifactKind,
    /// Unix mode, octal text such as `"0600"`.
    pub mode: String,
    pub action: FileAction,
    /// Whether applying replaces a file that exists with different text;
    /// `--apply` refuses that without `--force`.
    pub overwrites: bool,
    /// A secret: its content is never in the plan's output, only here in
    /// memory to be written.
    pub sensitive: bool,
    /// The text to write; `null` for a sensitive file.
    pub content: Option<String>,
    /// A unified diff against the existing file; `null` when unchanged or
    /// sensitive.
    pub diff: Option<String>,
    pub validation: Validation,
    /// For a server configuration: the flags it is served with
    /// (`by serve --config FILE FLAGS`), which its validation uses too.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub serve_flags: Vec<String>,
    /// For a harness install (`by init plugin`): how the packaged file is
    /// installed, such as `write` or `merge-json at hooks`. A merge only
    /// adds, so it never counts as replacing the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub how: Option<String>,
    /// Why this file cannot be installed (a merge that would have to
    /// replace something): the plan is then invalid and writes nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    /// A sensitive file's text, never serialized.
    #[serde(skip)]
    #[schemars(skip)]
    body: String,
}

impl PlannedFile {
    /// A file to write, diffed against `existing`.
    pub fn new(
        path: &str,
        kind: ArtifactKind,
        mode: u32,
        body: String,
        existing: Option<String>,
    ) -> PlannedFile {
        let (action, diff) = match &existing {
            None => (FileAction::Create, Some(unified_diff(path, "", &body))),
            Some(old) if *old == body => (FileAction::Unchanged, None),
            Some(old) => (FileAction::Update, Some(unified_diff(path, old, &body))),
        };
        PlannedFile {
            path: path.into(),
            kind,
            mode: format!("{mode:04o}"),
            action,
            overwrites: action == FileAction::Update,
            sensitive: false,
            content: Some(body.clone()),
            diff,
            validation: Validation::default(),
            serve_flags: Vec::new(),
            how: None,
            refusal: None,
            body,
        }
    }

    /// A harness install's file (`by init plugin`), `how` it is installed:
    /// written as packaged, or the existing file with a fragment merged
    /// in. Executable when it starts with `#!`.
    pub fn installed(
        path: &str,
        kind: ArtifactKind,
        body: String,
        existing: Option<String>,
        how: &str,
    ) -> PlannedFile {
        let mode = match body.starts_with("#!") {
            true => 0o755,
            false => 0o644,
        };
        let mut file = PlannedFile::new(path, kind, mode, body, existing);
        if how.starts_with("merge") {
            file.overwrites = false;
        }
        file.how = Some(how.into());
        file
    }

    /// A harness install's file that cannot be installed, and why.
    pub fn refused(path: &str, kind: ArtifactKind, how: &str, why: String) -> PlannedFile {
        let mut file = PlannedFile::new(path, kind, 0o644, String::new(), Some(String::new()));
        file.content = None;
        file.how = Some(how.into());
        file.refusal = Some(why);
        file
    }

    /// A generated secret, written 0600 when missing and kept when it
    /// exists. Its text never leaves memory except into the file.
    pub fn secret(path: &str, body: String, exists: bool) -> PlannedFile {
        PlannedFile {
            path: path.into(),
            kind: ArtifactKind::Secret,
            mode: "0600".into(),
            action: match exists {
                true => FileAction::Keep,
                false => FileAction::Create,
            },
            overwrites: false,
            sensitive: true,
            content: None,
            diff: None,
            validation: Validation::ok("generated secret"),
            serve_flags: Vec::new(),
            how: None,
            refusal: None,
            body: match exists {
                true => String::new(),
                false => body,
            },
        }
    }

    /// The bytes to write.
    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn mode_bits(&self) -> u32 {
        u32::from_str_radix(&self.mode, 8).unwrap_or(0o644)
    }

    /// Whether applying writes this file.
    pub fn writes(&self) -> bool {
        matches!(self.action, FileAction::Create | FileAction::Update)
    }
}

/// A command to run after applying.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FollowUp {
    pub command: String,
    pub why: String,
}

/// A topic's result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Plan {
    pub topic: crate::Topic,
    /// What the plan does, in a few lines.
    pub summary: Vec<String>,
    /// In the order they are written.
    pub files: Vec<PlannedFile>,
    /// Run after applying, in order; validation first.
    pub commands: Vec<FollowUp>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Every file passed its validator.
    pub valid: bool,
}

impl Plan {
    pub fn new(topic: crate::Topic) -> Plan {
        Plan {
            topic,
            summary: Vec::new(),
            files: Vec::new(),
            commands: Vec::new(),
            notes: Vec::new(),
            valid: true,
        }
    }

    pub fn command(&mut self, command: impl Into<String>, why: impl Into<String>) {
        self.commands.push(FollowUp {
            command: command.into(),
            why: why.into(),
        });
    }

    /// Like [`Plan::command`], but first in the list: what has to happen
    /// before anything else the plan suggests.
    pub fn command_first(&mut self, command: impl Into<String>, why: impl Into<String>) {
        self.commands.insert(
            0,
            FollowUp {
                command: command.into(),
                why: why.into(),
            },
        );
    }

    /// Run `validator` over every file and record the verdicts.
    pub fn validate(&mut self, validator: &dyn Validator) {
        let snapshot = self.files.clone();
        for file in &mut self.files {
            file.validation = match (&file.refusal, file.kind) {
                (Some(why), _) => {
                    Validation::failed(file.how.as_deref().unwrap_or("install"), why.clone())
                }
                (None, ArtifactKind::Secret) => Validation::ok("generated secret"),
                (None, _) => validator.validate(file, &snapshot),
            };
        }
        self.valid = self.files.iter().all(|f| f.validation.ok);
    }

    /// Files that applying would replace.
    pub fn overwrites(&self) -> Vec<&PlannedFile> {
        self.files.iter().filter(|f| f.overwrites).collect()
    }

    /// Show each diff's paths as [`diff_label`] does: a file under the
    /// home directory relative to it, instead of `b//home/dev/.claude/...`.
    /// The files' own paths, which are written, stay as they are.
    pub fn label_diffs(&mut self, home: Option<&str>) {
        for file in &mut self.files {
            let label = diff_label(&file.path, home);
            if label == file.path {
                continue;
            }
            if let Some(diff) = &file.diff {
                // Only the two header lines name the file.
                let headers = [
                    (format!("--- a/{}", file.path), format!("--- a/{label}")),
                    (format!("+++ b/{}", file.path), format!("+++ b/{label}")),
                ];
                let mut out = String::with_capacity(diff.len());
                for (i, line) in diff.split_inclusive('\n').enumerate() {
                    let bare = line.trim_end_matches('\n');
                    match headers.iter().find(|(old, _)| i < 2 && bare == old) {
                        Some((_, new)) => out.push_str(&line.replacen(bare, new, 1)),
                        None => out.push_str(line),
                    }
                }
                file.diff = Some(out);
            }
        }
    }
}

/// The path a diff header shows: under `home`, relative to it
/// (`.claude/settings.json`); any other absolute path without its leading
/// slash; a relative path as it is.
pub fn diff_label(path: &str, home: Option<&str>) -> String {
    let under_home = home
        .map(|h| h.trim_end_matches('/'))
        .filter(|h| !h.is_empty())
        .and_then(|h| path.strip_prefix(h))
        .and_then(|rest| rest.strip_prefix('/'));
    match under_home {
        Some(rest) => rest.to_owned(),
        None => path.trim_start_matches('/').to_owned(),
    }
}

/// Checks a planned file with the loader that will read it. The engine's
/// own [`BuiltinValidator`] knows the project configuration and skills;
/// `by` adds the server configuration, rigs and compose files.
pub trait Validator {
    /// `all` is the whole plan, so a file that references another (a
    /// server configuration and its token file) is checked with it.
    fn validate(&self, file: &PlannedFile, all: &[PlannedFile]) -> Validation;
}

/// The validators that need nothing outside this crate.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinValidator;

impl Validator for BuiltinValidator {
    fn validate(&self, file: &PlannedFile, _all: &[PlannedFile]) -> Validation {
        match file.kind {
            ArtifactKind::ProjectConfig => match crate::config::parse(file.body()) {
                Ok(_) => Validation::ok("branchyard.toml parser"),
                Err(e) => Validation::failed("branchyard.toml parser", e.to_string()),
            },
            ArtifactKind::Skill => {
                match branchyard_provision::skillset::check_skill_file(&file.path, file.body()) {
                    Ok(()) => Validation::ok("skill frontmatter"),
                    Err(e) => Validation::failed("skill frontmatter", e),
                }
            }
            ArtifactKind::Text => Validation::ok("text"),
            ArtifactKind::HarnessConfig => {
                let name = file.path.rsplit('/').next().unwrap_or(&file.path);
                match name.rsplit_once('.').map(|(_, ext)| ext) {
                    Some("json") => match serde_json::from_str::<serde_json::Value>(file.body()) {
                        Ok(_) => Validation::ok("json"),
                        Err(e) => Validation::failed("json", e.to_string()),
                    },
                    Some("toml") => match file.body().parse::<toml_edit::DocumentMut>() {
                        Ok(_) => Validation::ok("toml"),
                        Err(e) => Validation::failed("toml", e.to_string()),
                    },
                    _ => Validation::skipped("yaml", "not parsed: by has no YAML parser"),
                }
            }
            ArtifactKind::Secret => Validation::ok("generated secret"),
            ArtifactKind::ServerConfig => {
                match serde_json::from_str::<serde_json::Value>(file.body()) {
                    Ok(_) => Validation::skipped(
                        "json",
                        "only parsed as JSON; `by` checks it with the server's loader",
                    ),
                    Err(e) => Validation::failed("json", e.to_string()),
                }
            }
            ArtifactKind::Rig => {
                Validation::skipped("rig", "`by` checks rigs with `by rig check`'s planner")
            }
            ArtifactKind::Compose => {
                Validation::skipped("compose", "`by` checks compose files with docker compose")
            }
        }
    }
}

/// A unified diff of `old` to `new`, with `a/` and `b/` headers.
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let old_header = match old.is_empty() {
        true => "/dev/null".to_owned(),
        false => format!("a/{path}"),
    };
    diff.unified_diff()
        .context_radius(3)
        .header(&old_header, &format!("b/{path}"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_diff_against_what_exists() {
        let new = PlannedFile::new("a.toml", ArtifactKind::Text, 0o644, "x = 1\n".into(), None);
        assert_eq!(new.action, FileAction::Create);
        assert!(new.diff.as_deref().unwrap().contains("+x = 1"));
        let same = PlannedFile::new(
            "a.toml",
            ArtifactKind::Text,
            0o644,
            "x = 1\n".into(),
            Some("x = 1\n".into()),
        );
        assert_eq!(same.action, FileAction::Unchanged);
        assert!(!same.overwrites);
        let changed = PlannedFile::new(
            "a.toml",
            ArtifactKind::Text,
            0o644,
            "x = 2\n".into(),
            Some("x = 1\n".into()),
        );
        assert!(changed.overwrites);
        let diff = changed.diff.unwrap();
        assert!(
            diff.contains("--- a/a.toml") && diff.contains("-x = 1") && diff.contains("+x = 2"),
            "{diff}"
        );
    }

    #[test]
    fn diff_headers_name_a_file_under_the_home_relative_to_it() {
        let label = |path: &str, home: Option<&str>| diff_label(path, home);
        assert_eq!(
            label("/home/dev/.claude/settings.json", Some("/home/dev")),
            ".claude/settings.json"
        );
        assert_eq!(label("/home/dev/x", Some("/home/dev/")), "x");
        // Not under it (a sibling directory with the same prefix, another
        // root, no home, a home of "/"): no leading slash, never `b//`.
        assert_eq!(
            label("/home/developer/x", Some("/home/dev")),
            "home/developer/x"
        );
        assert_eq!(label("/etc/x.toml", Some("/home/dev")), "etc/x.toml");
        assert_eq!(label("/opt/x", None), "opt/x");
        assert_eq!(label("/home/dev/x", Some("/")), "home/dev/x");
        // A path in the repository is already relative.
        assert_eq!(
            label("branchyard.toml", Some("/home/dev")),
            "branchyard.toml"
        );
    }

    #[test]
    fn labelling_rewrites_only_the_two_header_lines() {
        let mut plan = Plan::new(crate::Topic::Plugin);
        let path = "/home/dev/.codex/hooks.json";
        // The text mentions its own path; only the headers change.
        let body = format!("{{\"note\": \"b/{path}\"}}\n");
        let created = PlannedFile::new(path, ArtifactKind::Text, 0o644, body.clone(), None);
        let updated = PlannedFile::new(
            path,
            ArtifactKind::Text,
            0o644,
            body.clone(),
            Some("{}\n".into()),
        );
        plan.files = vec![created, updated];
        plan.label_diffs(Some("/home/dev"));
        let created = plan.files[0].diff.as_deref().unwrap();
        assert!(
            created.starts_with("--- /dev/null\n+++ b/.codex/hooks.json\n"),
            "{created}"
        );
        assert!(
            created.contains(&format!("+{{\"note\": \"b/{path}\"}}")),
            "{created}"
        );
        let updated = plan.files[1].diff.as_deref().unwrap();
        assert!(
            updated.starts_with("--- a/.codex/hooks.json\n+++ b/.codex/hooks.json\n"),
            "{updated}"
        );
        assert_eq!(plan.files[0].path, path, "the path written is unchanged");
        // Without a home, an absolute path still loses its doubled slash.
        let mut plan = Plan::new(crate::Topic::Plugin);
        plan.files = vec![PlannedFile::new(
            "/opt/x/a.json",
            ArtifactKind::Text,
            0o644,
            "{}\n".into(),
            None,
        )];
        plan.label_diffs(None);
        assert!(plan.files[0]
            .diff
            .as_deref()
            .unwrap()
            .contains("+++ b/opt/x/a.json\n"));
    }

    #[test]
    fn a_command_can_lead_the_list() {
        let mut plan = Plan::new(crate::Topic::Project);
        plan.command("second", "b");
        plan.command_first("first", "a");
        let order: Vec<&str> = plan.commands.iter().map(|c| c.command.as_str()).collect();
        assert_eq!(order, ["first", "second"]);
    }

    #[test]
    fn secrets_never_serialize_their_text() {
        let secret = PlannedFile::secret("t.token", "s3cr3t-token-value".into(), false);
        let json = serde_json::to_string(&secret).unwrap();
        assert!(!json.contains("s3cr3t"), "{json}");
        assert_eq!(secret.body(), "s3cr3t-token-value");
        assert_eq!(secret.mode_bits(), 0o600);
        let kept = PlannedFile::secret("t.token", "other".into(), true);
        assert_eq!(kept.action, FileAction::Keep);
        assert!(!kept.writes());
    }
}
