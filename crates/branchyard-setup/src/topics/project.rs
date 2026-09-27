//! `project`: `branchyard.toml`, or the user's `config.toml`.

use serde_json::{json, Value};

use super::{flag, harness_question, list, secret_choices, secret_ref_question, text};
use crate::config::{self, PermissionsMode, ProjectConfig, ProviderKind};
use crate::interview::{number, Answers, Choice, Condition, Kind, Question, Rule};
use crate::plan::{ArtifactKind, Plan, PlannedFile};
use crate::probe::{Facts, Probe};
use crate::Topic;

const SECRET_PREFIX: &str = "secret.";

/// The file the answers go to, and what it holds now.
fn target(facts: &Facts, answers: &Answers) -> (String, Option<ProjectConfig>) {
    match text(answers, "scope").as_deref() {
        Some("user") => (
            facts.user_config_path.clone(),
            facts
                .user_text
                .as_deref()
                .and_then(|t| config::parse(t).ok()),
        ),
        _ => (
            config::PROJECT_FILE.to_owned(),
            facts
                .project_text
                .as_deref()
                .and_then(|t| config::parse(t).ok()),
        ),
    }
}

pub fn questions(facts: &Facts, answers: &Answers) -> Vec<Question> {
    let (_, existing) = target(facts, answers);
    let current = existing.clone().unwrap_or_default();
    let d = &current.defaults;
    let or = |value: Option<Value>, fallback: Value| value.unwrap_or(fallback);
    let harness = text(answers, "harness")
        .or_else(|| d.harness.clone())
        .unwrap_or_else(|| facts.preferred_harness());
    let mut qs = vec![
        Question::new(
            "scope",
            Kind::Select,
            "Scope",
            "Where should these defaults live?",
            "The project file is shared through the repository; the user file applies to every repository you run by in. The project file wins where both set a key.",
        )
        .default("project")
        .choices(vec![
            Choice::new(
                "project",
                "This repository",
                format!(
                    "{} at the repository root{}",
                    config::PROJECT_FILE,
                    match facts.project_config {
                        true => "; it exists and is updated",
                        false => "",
                    }
                ),
            ),
            Choice::new("user", "Just me", facts.user_config_path.clone()),
        ]),
        harness_question(
            "harness",
            "Harness",
            "Which harness should new branches use?",
            "by run uses it when you give no --harness.",
            facts,
            &harness,
        ),
        {
            let mut choices = vec![Choice::new(Value::Null, "Harness default", "whatever the harness picks")];
            if harness == "claude-code" {
                choices.extend([
                    Choice::new("large", "Large", "Claude Code's large alias"),
                    Choice::new("medium", "Medium", "Claude Code's medium alias"),
                    Choice::new("small", "Small", "Claude Code's small alias"),
                ]);
            }
            Question::new(
                "model",
                Kind::Text,
                "Model",
                "Which model should the harness use?",
                "Passed as --model: a model name, or a size alias where the harness defines one.",
            )
            .optional()
            .default(or(d.model.clone().map(Value::from), Value::Null))
            .choices(choices)
            .when(Condition::truthy("harness"))
        },
        Question::new(
            "permissions",
            Kind::Select,
            "Permissions",
            "How should tool permission requests be answered?",
            "Every tool call the harness wants to make reaches Branchyard; this is the default for --ask and --yes.",
        )
        .optional()
        .default(match d.permissions {
            Some(PermissionsMode::Yes) => json!("yes"),
            Some(PermissionsMode::Ask) | None => json!("ask"),
        })
        .choices(vec![
            Choice::new("ask", "Ask me", "prompt on the terminal for each request (--ask)"),
            Choice::new("yes", "Allow all", "allow every request (--yes); only for trusted tasks"),
            Choice::new(Value::Null, "Decide per run", "ask when a terminal is attached, else deny"),
        ]),
        Question::new(
            "effort",
            Kind::Select,
            "Effort",
            "How hard should Codex reason?",
            "Passed as --effort; Codex takes it, Claude Code refuses it.",
        )
        .optional()
        .default(or(d.effort.clone().map(Value::from), Value::Null))
        .choices(vec![
            Choice::new(Value::Null, "Codex default", "leave it to Codex"),
            Choice::new("medium", "Medium", "balanced"),
            Choice::new("high", "High", "slower and more thorough"),
            Choice::new("low", "Low", "fastest"),
            Choice::new("xhigh", "Extra high", "slowest"),
        ])
        .when(Condition::equals("harness", "codex")),
        Question::new(
            "budget_usd",
            Kind::Number,
            "Budget",
            "What should one branch cost at most, in dollars?",
            "--budget-usd: a branch stops once its harness's own cost estimate passes it.",
        )
        .optional()
        .default(or(d.budget_usd.map(number), json!(5)))
        .choices(vec![
            Choice::new(2, "$2", "small fixes"),
            Choice::new(5, "$5", "a typical change"),
            Choice::new(20, "$20", "large changes"),
            Choice::new(Value::Null, "No limit", "only the harness's own limits apply"),
        ])
        .rule(Rule::Min { value: 0.01 }),
        Question::new(
            "max_turns",
            Kind::Number,
            "Turns",
            "How many turns may one branch take?",
            "--max-turns: a branch stops after this many.",
        )
        .optional()
        .default(or(d.max_turns.map(Value::from), Value::Null))
        .choices(vec![
            Choice::new(10, "10", "short tasks"),
            Choice::new(25, "25", "longer tasks"),
            Choice::new(Value::Null, "No limit", "no turn limit"),
        ])
        .rule(Rule::Min { value: 1.0 })
        .rule(Rule::Integer),
        Question::new(
            "max_minutes",
            Kind::Number,
            "Duration",
            "How many minutes may one turn run?",
            "--max-minutes: a turn is interrupted after this long.",
        )
        .optional()
        .default(or(d.max_minutes.map(number), json!(30)))
        .choices(vec![
            Choice::new(15, "15 minutes", "quick turns"),
            Choice::new(30, "30 minutes", "most turns"),
            Choice::new(60, "60 minutes", "long builds or test suites"),
            Choice::new(Value::Null, "No limit", "never interrupt a turn for its duration"),
        ])
        .rule(Rule::Min { value: 0.1 }),
        {
            let suggested = d.check.clone().or_else(|| facts.suggested_check.clone());
            let mut choices = Vec::new();
            if let Some(check) = &suggested {
                choices.push(Choice::new(check.as_str(), check.as_str(), "run before a merge"));
            }
            choices.push(Choice::new(Value::Null, "No check", "merge without running a check"));
            Question::new(
                "check",
                Kind::Text,
                "Check",
                "Which command must pass before a branch merges?",
                "--check: by merge runs it on the exact merge result and refuses a failure.",
            )
            .optional()
            .default(suggested.map(Value::from).unwrap_or(Value::Null))
            .choices(choices)
            .rule(Rule::CommandLine)
        },
        Question::new(
            "isolated",
            Kind::Confirm,
            "Isolation",
            "Should harnesses run isolated, with a private HOME and a scrubbed environment?",
            "--isolated keeps your credentials and dotfiles from the harness; it is then not logged in, so it needs a secret.",
        )
        .default(d.isolated.unwrap_or(false))
        .choices(vec![
            Choice::new(false, "No", "use your own HOME and login (no isolation beyond your user)"),
            Choice::new(true, "Yes", "private HOME; give the harness a secret next"),
        ]),
    ];
    if facts.platform.kvm || d.provider == Some(ProviderKind::Microsandbox) {
        qs.push(
            Question::new(
                "provider",
                Kind::Select,
                "Provider",
                "Where should harnesses run?",
                "KVM is available, so Microsandbox can run each turn in a microVM (a build with the microsandbox feature).",
            )
            .default(match d.provider {
                Some(ProviderKind::Microsandbox) => "microsandbox",
                _ => "local",
            })
            .choices(vec![
                Choice::new("local", "Local process", "in its own worktree, as your user"),
                Choice::new("microsandbox", "Microsandbox", "a microVM per turn from an OCI image"),
            ]),
        );
        qs.push(
            Question::new(
                "microsandbox.image",
                Kind::Text,
                "Image",
                "Which OCI image has the harness installed?",
                "--image: the microVM boots from it.",
            )
            .default(or(
                current
                    .microsandbox
                    .as_ref()
                    .map(|m| Value::from(m.image.clone())),
                Value::Null,
            ))
            .when(Condition::equals("provider", "microsandbox")),
        );
    }
    let needs_secrets = Condition::Any(vec![
        Condition::truthy("isolated"),
        Condition::equals("provider", "microsandbox"),
    ]);
    let (choices, set) = secret_choices(facts, std::slice::from_ref(&harness));
    let existing_secrets: Vec<Value> = current
        .secrets
        .keys()
        .map(|k| Value::from(k.as_str()))
        .collect();
    qs.push(
        Question::new(
            "secrets",
            Kind::Multiselect,
            "Secrets",
            "Which credentials should the harness get?",
            "An isolated harness has no login; these are written 0600 into its private home. Only names are stored, never values.",
        )
        .optional()
        .default(match existing_secrets.is_empty() {
            false => Value::Array(existing_secrets),
            true => Value::Array(set.into_iter().map(Value::from).collect()),
        })
        .choices(choices)
        .allow_other(true)
        .rule(Rule::SecretRef)
        .when(needs_secrets),
    );
    for name in list(answers, "secrets") {
        let mut q = secret_ref_question(
            SECRET_PREFIX,
            &name,
            facts,
            "Only where it is goes in the file: a variable name, or @path to a file.",
        )
        .when(Condition::includes("secrets", name.as_str()));
        if let Some(source) = current.secrets.get(&name) {
            q.default = Value::from(source.as_str());
        }
        qs.push(q);
    }
    let remote = current.remote.url.clone();
    qs.push(
        Question::new(
            "remote",
            Kind::Select,
            "Server",
            "Should by run here, or against a Branchyard server?",
            "With a server, every by command runs there (--remote), with the same output.",
        )
        .default(match remote {
            Some(_) => "server",
            None => "local",
        })
        .choices(vec![
            Choice::new(
                "local",
                "This machine",
                "local mode: branches are worktrees here",
            ),
            Choice::new("server", "A server", "set [remote]: URL and token file"),
        ]),
    );
    qs.push(
        Question::new(
            "remote.url",
            Kind::Text,
            "Server URL",
            "What is the server's URL?",
            "--remote; BRANCHYARD_REMOTE overrides it.",
        )
        .default(
            current
                .remote
                .url
                .clone()
                .unwrap_or_else(|| "http://127.0.0.1:8421".into()),
        )
        .choices(vec![
            Choice::new(
                "http://127.0.0.1:8421",
                "Local server",
                "by serve on this machine",
            ),
            Choice::new(
                "https://branchyard.example.com",
                "Remote HTTPS",
                "replace with your server's URL",
            ),
        ])
        .rule(Rule::Url {
            schemes: vec!["http".into(), "https".into()],
        })
        .when(Condition::equals("remote", "server")),
    );
    qs.push(
        Question::new(
            "remote.token_file",
            Kind::Path,
            "Token file",
            "Which file holds your bearer token?",
            "--token-file: the token stays in that file; only its path is stored.",
        )
        .default(
            current
                .remote
                .token_file
                .clone()
                .unwrap_or_else(|| ".branchyard/server/token".into()),
        )
        .choices(vec![
            Choice::new(
                ".branchyard/server/token",
                "by serve's token",
                "what by serve creates by default",
            ),
            Choice::new(
                ".branchyard/tokens/admin.token",
                "by init server's",
                "the admin token by init server writes",
            ),
        ])
        .when(Condition::equals("remote", "server")),
    );
    qs.push(
        Question::new(
            "remote.ca_file",
            Kind::Path,
            "CA file",
            "Which CA certificates should verify the server, if not the system's?",
            "--ca-file, for a server with a private certificate authority.",
        )
        .optional()
        .default(or(
            current.remote.ca_file.clone().map(Value::from),
            Value::Null,
        ))
        .choices(vec![Choice::new(
            Value::Null,
            "System roots",
            "the usual public authorities",
        )])
        .when(Condition::equals("remote", "server")),
    );
    qs
}

pub fn plan(facts: &Facts, answers: &Answers, probe: &dyn Probe) -> Plan {
    let (path, existing) = target(facts, answers);
    let mut flat = existing.clone().unwrap_or_default().flatten();
    fn put(flat: &mut std::collections::BTreeMap<String, Value>, key: &str, value: Option<Value>) {
        match value {
            Some(value) if !value.is_null() => {
                flat.insert(key.to_owned(), value);
            }
            _ => {
                flat.remove(key);
            }
        }
    }
    macro_rules! set {
        ($key:expr, $value:expr) => {
            put(&mut flat, $key, $value)
        };
    }
    let get = |id: &str| answers.get(id).cloned();
    set!("version", Some(json!(config::VERSION)));
    set!("defaults.harness", get("harness"));
    set!("defaults.model", get("model"));
    set!("defaults.permissions", get("permissions"));
    set!("defaults.effort", get("effort"));
    set!("defaults.budget_usd", get("budget_usd"));
    set!("defaults.max_turns", get("max_turns"));
    set!("defaults.max_minutes", get("max_minutes"));
    set!("defaults.check", get("check"));
    set!(
        "defaults.isolated",
        Some(Value::Bool(flag(answers, "isolated")))
    );
    if answers.contains_key("provider") {
        set!("defaults.provider", get("provider"));
    }
    let sandbox = text(answers, "provider").as_deref() == Some("microsandbox");
    if sandbox {
        set!("microsandbox.image", get("microsandbox.image"));
    }
    // Secrets apply only with a private home; without one, keep whatever
    // the file already had, since this interview did not ask.
    if flag(answers, "isolated") || sandbox {
        flat.retain(|key, _| !key.starts_with("secrets."));
        for name in list(answers, "secrets") {
            let source =
                text(answers, &format!("{SECRET_PREFIX}{name}")).unwrap_or_else(|| name.clone());
            flat.insert(format!("secrets.{name}"), Value::from(source));
        }
    }
    match text(answers, "remote").as_deref() {
        Some("server") => {
            set!("remote.url", get("remote.url"));
            set!("remote.token_file", get("remote.token_file"));
            set!("remote.ca_file", get("remote.ca_file"));
        }
        _ => {
            for key in [
                "remote.url",
                "remote.token_file",
                "remote.ca_file",
                "remote.repo",
            ] {
                flat.remove(key);
            }
        }
    }
    let mut plan = Plan::new(Topic::Project);
    let body = match ProjectConfig::unflatten(&flat) {
        Ok(config) => {
            let heading = match path.as_str() {
                config::PROJECT_FILE => {
                    "Branchyard project defaults, written by `by init project`."
                }
                _ => "Branchyard user defaults, written by `by init project`.",
            };
            config::render(&config, heading)
        }
        // Rendered anyway so validation reports the problem by key.
        Err(e) => format!("# by init project could not build this file: {e}\n"),
    };
    plan.summary.push(format!(
        "{} {path} with defaults for new branches: harness {}{}",
        match probe.exists(&path) {
            true => "Update",
            false => "Create",
        },
        text(answers, "harness").unwrap_or_default(),
        match text(answers, "remote").as_deref() {
            Some("server") => format!(
                ", against {}",
                text(answers, "remote.url").unwrap_or_default()
            ),
            _ => ", in local mode".into(),
        }
    ));
    if existing.is_some() {
        plan.notes.push(
            "Keys the interview does not ask about are kept; comments in the old file are not."
                .into(),
        );
    }
    let secret_note = list(answers, "secrets");
    if !secret_note.is_empty() {
        plan.notes.push(format!(
            "Secrets by name only ({}); their values stay in their variables or files.",
            secret_note.join(", ")
        ));
    }
    plan.files.push(PlannedFile::new(
        &path,
        ArtifactKind::ProjectConfig,
        0o644,
        body,
        probe.read(&path),
    ));
    plan.command(
        "by config validate",
        "Check the merged configuration with the loader by uses.",
    );
    plan.command(
        "by config show",
        "See every effective value and where it came from.",
    );
    if text(answers, "remote").as_deref() == Some("server") {
        plan.command("by ls", "Reach the server with the new [remote] settings.");
    } else {
        plan.command("by harnesses", "Confirm the default harness is installed.");
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interview::resolve;
    use crate::probe::FakeProbe;
    use std::collections::BTreeMap;

    #[test]
    fn detected_harnesses_and_check_become_defaults() {
        let facts = Facts::gather(&FakeProbe::typical());
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        assert!(state.done(), "{state:?}");
        assert_eq!(state.answers["harness"], json!("claude-code"));
        assert_eq!(state.answers["check"], json!("cargo test"));
        assert_eq!(
            state.answers["effort"],
            Value::Null,
            "effort only applies to codex"
        );
        assert_eq!(state.answers["remote.url"], Value::Null);
    }

    #[test]
    fn secrets_are_asked_per_name_only_when_isolated() {
        let facts = Facts::gather(&FakeProbe::typical());
        let raw: BTreeMap<String, Value> =
            serde_json::from_value(json!({"isolated": "yes", "secrets": "ANTHROPIC_API_KEY"}))
                .unwrap();
        let state = resolve(&|a| questions(&facts, a), &raw, false);
        assert!(state
            .pending
            .iter()
            .any(|q| q.id == "secret.ANTHROPIC_API_KEY" && q.kind == Kind::SecretRef));
    }
}
