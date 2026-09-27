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
            body,
        }
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

    /// Run `validator` over every file and record the verdicts.
    pub fn validate(&mut self, validator: &dyn Validator) {
        let snapshot = self.files.clone();
        for file in &mut self.files {
            file.validation = match file.kind {
                ArtifactKind::Secret => Validation::ok("generated secret"),
                _ => validator.validate(file, &snapshot),
            };
        }
        self.valid = self.files.iter().all(|f| f.validation.ok);
    }

    /// Files that applying would replace.
    pub fn overwrites(&self) -> Vec<&PlannedFile> {
        self.files.iter().filter(|f| f.overwrites).collect()
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
            ArtifactKind::Skill => match crate::skills::check_skill_file(&file.path, file.body()) {
                Ok(()) => Validation::ok("skill frontmatter"),
                Err(e) => Validation::failed("skill frontmatter", e),
            },
            ArtifactKind::Text => Validation::ok("text"),
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
