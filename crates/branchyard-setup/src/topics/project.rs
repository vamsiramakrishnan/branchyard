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
    // A recipe chosen by hand is kept: this question offers only local and
    // Microsandbox.
    if (facts.platform.kvm || d.provider == Some(ProviderKind::Microsandbox))
        && d.provider != Some(ProviderKind::Recipe)
    {
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
    workspace_questions(facts, &current, &mut qs);
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

/// `[workspace]`, for the project file only: detected install commands,
/// `.env` files, a dev server on the branch's port, and a Compose stack's
/// teardown, each a default the person confirms.
fn workspace_questions(facts: &Facts, current: &ProjectConfig, qs: &mut Vec<Question>) {
    let detected = &facts.workspace;
    let existing = current.workspace.as_ref();
    let project = Condition::equals("scope", "project");
    let enabled = Condition::All(vec![project.clone(), Condition::truthy("workspace")]);
    let joined = |commands: Vec<String>| match commands.is_empty() {
        true => None,
        false => Some(commands.join(" && ")),
    };
    let setup = existing
        .map(|w| joined(w.setup.commands()))
        .unwrap_or_else(|| joined(detected.setup.clone()));
    let teardown = existing
        .map(|w| joined(w.teardown.commands()))
        .unwrap_or_else(|| joined(detected.teardown.clone()));
    let run = match existing {
        Some(w) => w.run_script(None).ok().and_then(|(_, c)| joined(c)),
        None => detected.run.clone(),
    };
    let copy: Vec<String> = existing
        .map(|w| w.copy.clone())
        .unwrap_or_else(|| detected.copy.clone());
    qs.push(
        Question::new(
            "workspace",
            Kind::Confirm,
            "Workspace",
            "Should each new branch's worktree be made ready before its first turn?",
            "[workspace] copies untracked files such as .env into the worktree, runs setup (an install) and gives it BRANCHYARD_PORT; its scripts run only after `by workspace trust`.",
        )
        .default(existing.is_some() || !detected.is_empty())
        .choices(vec![
            Choice::new(true, "Yes", "copy files and run setup in each new worktree"),
            Choice::new(false, "No", "a bare worktree, as now"),
        ])
        .when(project),
    );
    let mut copy_choices: Vec<Choice> = copy
        .iter()
        .chain(detected.copy.iter())
        .fold(Vec::new(), |mut seen: Vec<&String>, file| {
            if !seen.contains(&file) {
                seen.push(file);
            }
            seen
        })
        .into_iter()
        .take(3)
        .map(|file| Choice::new(file.as_str(), file.as_str(), "found at the repository root"))
        .collect();
    copy_choices.push(Choice::new(".env*", ".env*", "every .env file at the root"));
    qs.push(
        Question::new(
            "workspace.copy",
            Kind::Multiselect,
            "Copy",
            "Which untracked files should each new worktree get a copy of?",
            "Globs relative to the repository root; never outside it, never a symbolic link, and never committed from the branch.",
        )
        .optional()
        .default(Value::Array(copy.into_iter().map(Value::from).collect()))
        .choices(copy_choices)
        .allow_other(true)
        .rule(Rule::CopyGlob)
        .when(enabled.clone()),
    );
    let command =
        |id: &str, header: &str, prompt: &str, why: &str, value: Option<String>, none: &str| {
            let mut choices = Vec::new();
            if let Some(value) = &value {
                choices.push(Choice::new(
                    value.as_str(),
                    value.as_str(),
                    "detected from the repository's files",
                ));
            }
            choices.push(Choice::new(Value::Null, none, "nothing runs"));
            Question::new(id, Kind::Text, header, prompt, why)
                .optional()
                .default(value.map(Value::from).unwrap_or(Value::Null))
                .choices(choices)
                .allow_other(true)
                .rule(Rule::CommandLine)
                .when(enabled.clone())
        };
    qs.push(command(
        "workspace.setup",
        "Setup",
        "What should run in each new worktree before its first turn?",
        "Runs with sh -c in the worktree; a failure fails the branch with its output in by log. It must be safe to run twice.",
        setup,
        "No setup",
    ));
    qs.push(command(
        "workspace.run",
        "Run",
        "Which command starts a development server in a branch?",
        "`by workspace run BRANCH` runs it in the branch's worktree, with its own port in $BRANCHYARD_PORT.",
        run,
        "No run script",
    ));
    qs.push(command(
        "workspace.teardown",
        "Teardown",
        "What should run when a branch is removed?",
        "Runs in the worktree on by rm and by merge --rm, best-effort, such as stopping the branch's containers.",
        teardown,
        "No teardown",
    ));
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
    let workspace =
        text(answers, "scope").as_deref() != Some("user") && answers.contains_key("workspace");
    if workspace {
        let before = existing.as_ref().and_then(|c| c.workspace.clone());
        flat.retain(|key, _| key != "workspace" && !key.starts_with("workspace."));
        if flag(answers, "workspace") {
            flat.insert("workspace".into(), json!({}));
            let copy = list(answers, "workspace.copy");
            if !copy.is_empty() {
                flat.insert("workspace.copy".into(), json!(copy));
            }
            set!("workspace.setup", get("workspace.setup"));
            set!("workspace.teardown", get("workspace.teardown"));
            // Other run scripts are kept; the asked one is the default.
            let mut runs = before.map(|w| w.run).unwrap_or_default();
            let name = runs
                .iter()
                .find(|(_, r)| r.default)
                .map(|(name, _)| name.clone())
                .unwrap_or_else(|| "dev".into());
            match text(answers, "workspace.run") {
                Some(command) => {
                    runs.insert(
                        name,
                        config::RunScript {
                            command: config::Script::One(command),
                            default: true,
                        },
                    );
                }
                None => {
                    runs.remove(&name);
                }
            }
            if !runs.is_empty() {
                flat.insert(
                    "workspace.run".into(),
                    serde_json::to_value(&runs).unwrap_or_default(),
                );
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
    if workspace && flag(answers, "workspace") {
        plan.notes.push(
            "[workspace] scripts run only once you trust them: review them, then run \
             `by workspace trust`. A change to them asks again."
                .into(),
        );
        plan.command(
            "by workspace trust",
            "Let [workspace]'s setup, run and teardown scripts run for this repository.",
        );
    }
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
    fn a_workspace_is_suggested_from_the_repositorys_files() {
        let mut probe = FakeProbe::typical();
        for file in [
            "package.json",
            "pnpm-lock.yaml",
            ".env",
            ".env.local",
            "compose.yaml",
        ] {
            probe.files.insert(file.into(), String::new());
        }
        let facts = Facts::gather(&probe);
        assert_eq!(facts.workspace.copy, [".env", ".env.local"]);
        assert_eq!(
            facts.workspace.setup[..2],
            [
                "pnpm install --frozen-lockfile".to_owned(),
                "cargo fetch".to_owned()
            ]
        );
        assert!(facts.workspace.setup[2].starts_with("docker compose -p"));
        assert_eq!(
            facts.workspace.run.as_deref(),
            Some("PORT=$BRANCHYARD_PORT pnpm dev")
        );
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        assert!(state.done(), "{state:?}");
        assert_eq!(state.answers["workspace"], json!(true));
        assert_eq!(
            state.answers["workspace.copy"],
            json!([".env", ".env.local"])
        );
        let plan = plan(&facts, &state.answers, &probe);
        let body = plan.files[0].content.clone().unwrap();
        let parsed = config::parse(&body).unwrap();
        let workspace = parsed.workspace.unwrap();
        assert_eq!(workspace.copy, [".env", ".env.local"]);
        assert!(workspace.setup.commands()[0]
            .starts_with("pnpm install --frozen-lockfile && cargo fetch && docker compose"));
        assert_eq!(workspace.run_script(None).unwrap().0, "dev");
        assert!(workspace.teardown.commands()[0].ends_with(" down"));
        assert!(plan
            .commands
            .iter()
            .any(|c| c.command == "by workspace trust"));

        // Declining writes none, and the user scope never asks.
        let mut declined = state.answers.clone();
        declined.insert("workspace".into(), json!(false));
        let body = plan_body(&facts, &declined, &probe);
        assert!(config::parse(&body).unwrap().workspace.is_none(), "{body}");
        let user: BTreeMap<String, Value> =
            serde_json::from_value(json!({"scope": "user"})).unwrap();
        let state = resolve(&|a| questions(&facts, a), &user, true);
        assert!(!state.answers.contains_key("workspace") || state.answers["workspace"].is_null());
    }

    #[test]
    fn a_workspace_is_imported_from_another_tools_configuration() {
        let mut probe = FakeProbe::typical();
        for file in ["package.json", "pnpm-lock.yaml", ".env"] {
            probe.files.insert(file.into(), String::new());
        }
        probe.files.insert(
            ".emdash.json".into(),
            r#"{"preservePatterns": [".env.local"], "shellSetup": "nvm use",
                "scripts": {"setup": "pnpm install && pnpm build", "run": "PORT=$EMDASH_PORT pnpm dev"}}"#
                .into(),
        );
        // Superset gives teardown, which emdash's file does not; its setup
        // loses to emdash's, which comes first.
        probe.files.insert(
            ".superset/config.json".into(),
            r#"{"setup": ["bun install"], "teardown": ["docker compose down"]}"#.into(),
        );
        probe
            .files
            .insert(".conductor/settings.toml".into(), "[scripts\n".into());
        let facts = Facts::gather(&probe);
        let w = &facts.workspace;
        assert_eq!(w.setup, ["pnpm install && pnpm build"]);
        assert_eq!(w.run.as_deref(), Some("PORT=$BRANCHYARD_PORT pnpm dev"));
        assert_eq!(w.teardown, ["docker compose down"]);
        assert_eq!(w.copy, [".env", ".env.local"]);
        assert!(w
            .found
            .ends_with(&[".emdash.json".into(), ".superset/config.json".into()]));
        assert_eq!(w.notes.len(), 2, "{:?}", w.notes);
        assert!(w.notes[0].starts_with(".emdash.json: shellSetup is not imported"));
        assert!(w.notes[1].starts_with(".conductor/settings.toml was not imported:"));
        let lines = facts.lines();
        assert!(lines.iter().any(|l| l.label == "Not imported"), "{lines:?}");
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        let plan = plan(&facts, &state.answers, &probe);
        let parsed = config::parse(&plan_body(&facts, &state.answers, &probe)).unwrap();
        let workspace = parsed.workspace.unwrap();
        assert_eq!(workspace.setup.commands(), ["pnpm install && pnpm build"]);
        assert_eq!(workspace.copy, [".env", ".env.local"]);
        assert_eq!(
            workspace.run_script(None).unwrap().1,
            ["PORT=$BRANCHYARD_PORT pnpm dev"]
        );
        assert_eq!(workspace.teardown.commands(), ["docker compose down"]);
        assert!(plan.valid);
    }

    fn plan_body(facts: &Facts, answers: &Answers, probe: &FakeProbe) -> String {
        plan(facts, answers, probe).files[0]
            .content
            .clone()
            .unwrap()
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
