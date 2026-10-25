//! `project`: `branchyard.toml`, or the user's `config.toml`.

use serde_json::{json, Value};

use super::{flag, harness_question, list, secret_choices, secret_ref_question, text};
use crate::config::{self, ProjectConfig, ProviderKind};
use crate::hint;
use crate::interview::{number, Answers, Choice, Condition, Kind, Question, Rule};
use crate::plan::{ArtifactKind, Plan, PlannedFile};
use crate::probe::{harness_label, unapproved_tools, Facts, HarnessFact, LoginFile, Probe};
use crate::Topic;

const SECRET_PREFIX: &str = "secret.";

/// The file the answers go to, and what it holds now.
struct Target {
    path: String,
    /// What the file says, when it loads.
    existing: Option<ProjectConfig>,
    /// Why it does not load, when it exists and `by` refuses it: its
    /// values are then not used, and the plan replaces the file.
    unreadable: Option<hint::Unreadable>,
}

fn target(facts: &Facts, answers: &Answers) -> Target {
    let (path, text) = match text(answers, "scope").as_deref() {
        Some("user") => (facts.user_config_path.clone(), facts.user_text.as_deref()),
        _ => (
            config::PROJECT_FILE.to_owned(),
            facts.project_text.as_deref(),
        ),
    };
    Target {
        existing: text.and_then(|t| config::parse(t).ok()),
        unreadable: text.and_then(hint::unreadable),
        path,
    }
}

pub fn questions(facts: &Facts, answers: &Answers) -> Vec<Question> {
    let existing = target(facts, answers).existing;
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
            Some(mode) => json!(mode.as_str()),
            None => json!("ask"),
        })
        .choices(vec![
            Choice::new("ask", "Ask me", "prompt on the terminal for each request (--ask)"),
            Choice::new("yes", "Allow all", "allow every request (--yes); only for trusted tasks"),
            Choice::new(
                "read-only",
                "Read only",
                "read and search; edits, commands and the web denied (a preset)",
            ),
            Choice::new(
                "edit-worktree",
                "Edit files",
                "read, search and edit files; commands and the web denied (a preset)",
            ),
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
            .default(suggested.map_or(Value::Null, Value::from))
            .choices(choices)
            .rule(Rule::CommandLine)
        },
        Question::new(
            "isolated",
            Kind::Confirm,
            "Isolation",
            "Should harnesses run isolated, with a private HOME and a scrubbed environment?",
            "--isolated keeps your credentials and dotfiles from the harness; it is then not logged in, so it needs a secret. It is not a sandbox: the harness still runs as your user.",
        )
        .default(d.isolated.unwrap_or(false))
        .choices(vec![
            Choice::new(false, "No", "use your own HOME and login (no isolation beyond your user)"),
            Choice::new(true, "Yes", "private HOME, not a sandbox; give the harness a secret next"),
        ]),
    ];
    // A recipe chosen by hand is kept: this question offers only local and
    // Microsandbox. Microsandbox is offered where this build can run it and
    // the host has KVM; a file that already names it keeps the choice, and
    // says what the build lacks.
    if ((facts.platform.kvm && facts.build.microsandbox)
        || d.provider == Some(ProviderKind::Microsandbox))
        && d.provider != Some(ProviderKind::Recipe)
    {
        qs.push(
            Question::new(
                "provider",
                Kind::Select,
                "Provider",
                "Where should harnesses run?",
                &match facts.build.microsandbox {
                    true => "KVM is available, so Microsandbox can run each turn in a microVM.".to_owned(),
                    false => format!(
                        "This build has no Microsandbox support: rebuild with `{}` (needs KVM and libcap-ng).",
                        crate::probe::REBUILD_MICROSANDBOX
                    ),
                },
            )
            .default(match d.provider {
                Some(ProviderKind::Microsandbox) => "microsandbox",
                _ => "local",
            })
            .choices(vec![
                Choice::new("local", "Local process", "in its own worktree, as your user"),
                Choice::new(
                    "microsandbox",
                    "Microsandbox",
                    match facts.build.microsandbox {
                        true => "a microVM per turn from an OCI image",
                        false => "needs a build with --features microsandbox",
                    },
                ),
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
    login_question(facts, &current, answers, &mut qs);
    fleet_question(facts, answers, &mut qs);
    models_questions(facts, &current, &mut qs);
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

/// Asked once isolation is decided, whatever it is.
fn after_isolation() -> Condition {
    Condition::In {
        id: "isolated".into(),
        values: vec![Value::Bool(true), Value::Bool(false)],
    }
}

/// Whether new branches get a private home by default: isolated, or a
/// sandbox.
fn private_home(answers: &Answers) -> bool {
    flag(answers, "isolated") || text(answers, "provider").as_deref() == Some("microsandbox")
}

/// The source a login file is recorded under: `@~/.codex/auth.json`.
fn login_source(login: &LoginFile) -> String {
    format!("@{}", login.file)
}

/// The logins harnesses keep in files that a provisioner can carry into a
/// private home: recorded in `[secrets]` by name with where the file is
/// (`CODEX_AUTH = "@~/.codex/auth.json"`), never its content. Suggested
/// for installed harnesses when new branches get a private home, since
/// they have no login there otherwise.
fn login_question(
    facts: &Facts,
    current: &ProjectConfig,
    answers: &Answers,
    qs: &mut Vec<Question>,
) {
    let logins: Vec<(&LoginFile, &str)> = facts
        .logins
        .iter()
        .filter_map(|l| l.secret.as_deref().map(|secret| (l, secret)))
        .collect();
    if logins.is_empty() {
        return;
    }
    let installed = |harness: &str| facts.installed().any(|h| h.harness == harness);
    let default: Vec<Value> = logins
        .iter()
        .filter(|(login, secret)| match current.secrets.get(*secret) {
            Some(source) => *source == login_source(login),
            None => private_home(answers) && installed(&login.harness),
        })
        .map(|(_, secret)| Value::from(*secret))
        .collect();
    let choices = logins
        .iter()
        .map(|(login, secret)| {
            Choice::new(
                *secret,
                format!("{} login", harness_label(&login.harness)),
                format!(
                    "{}{}, as {secret}",
                    login.file,
                    login
                        .label
                        .as_ref()
                        .map_or(String::new(), |l| format!(" ({l})"))
                ),
            )
        })
        .collect();
    qs.push(
        Question::new(
            "logins",
            Kind::Multiselect,
            "Logins",
            "Which harness logins should isolated and sandboxed branches be given?",
            "A branch with a private home (--isolated, a sandbox) is not logged in. Each login \
             chosen is recorded in [secrets] by name with where its file is (CODEX_AUTH = \
             \"@~/.codex/auth.json\"), never its content, and copied 0600 into such a branch's \
             home when it starts. Branches that use your own home are not affected.",
        )
        .optional()
        .default(Value::Array(default))
        .choices(choices)
        .when(after_isolation()),
    );
}

/// The inventory default as `by init project` writes it: the installed
/// harnesses logged in here (as `by harnesses` tells), `harness` first,
/// effort `medium` where the harness takes one for the chosen isolation,
/// and the first that routes tool permission requests as the judge. The
/// same rules as `branchyard::inventory_default` (docs/fleet.md).
fn inventory_fleet(facts: &Facts, answers: &Answers) -> Option<config::FleetConfig> {
    let mut logged_in: Vec<&HarnessFact> = facts.logged_in().collect();
    if let Some(preferred) = text(answers, "harness") {
        if let Some(at) = logged_in.iter().position(|h| h.harness == preferred) {
            let first = logged_in.remove(at);
            logged_in.insert(0, first);
        }
    }
    let candidates: Vec<config::CandidateConfig> = logged_in
        .iter()
        .map(|h| config::CandidateConfig {
            harness: h.harness.clone(),
            effort: branchyard_harness::profiles::by_id(&h.profile)
                .is_some_and(|p| {
                    branchyard_provision::takes_effort(p.harness, p.protocol, private_home(answers))
                })
                .then(|| "medium".to_owned()),
            ..config::CandidateConfig::default()
        })
        .collect();
    let judge = candidates
        .iter()
        .find(|c| !unapproved_tools(&c.harness))
        .map(|c| config::JudgeConfig {
            harness: c.harness.clone(),
            effort: c.effort.clone(),
            ..config::JudgeConfig::default()
        });
    (!candidates.is_empty()).then_some(config::FleetConfig {
        candidates,
        judge,
        ..config::FleetConfig::default()
    })
}

/// The candidates' labels: `codex, gemini-cli`.
fn fleet_names(fleet: &config::FleetConfig) -> String {
    fleet
        .candidates
        .iter()
        .map(|c| match &c.effort {
            Some(effort) => format!("{} effort={effort}", c.harness),
            None => c.harness.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether neither file has a `[fleet]`: the inventory default is in use.
fn no_fleet(facts: &Facts) -> bool {
    [&facts.project_text, &facts.user_text]
        .into_iter()
        .flatten()
        .filter_map(|text| config::parse(text).ok())
        .all(|c| c.fleet.is_empty())
}

/// Offer to write the inventory default as an explicit `[fleet.default]`,
/// so it stops moving with the logins. Off by default: a `[fleet]` also
/// routes `by run` without `--harness`.
fn fleet_question(facts: &Facts, answers: &Answers, qs: &mut Vec<Question>) {
    if !no_fleet(facts) {
        return;
    }
    let Some(fleet) = inventory_fleet(facts, answers) else {
        return;
    };
    let names = fleet_names(&fleet);
    let judge = fleet
        .judge
        .as_ref()
        .map_or("deterministic".to_owned(), |j| j.harness.clone());
    qs.push(
        Question::new(
            "fleet",
            Kind::Confirm,
            "Fleet",
            "Should the harnesses logged in here be written down as [fleet.default]?",
            &format!(
                "Without a [fleet], by run --auto and by fan --auto route among the harnesses \
                 logged in at that moment (now {names}; judge {judge}), so the list moves as \
                 logins change. Written down it stays until you edit it, and by run without \
                 --harness routes too instead of using the default harness."
            ),
        )
        .default(false)
        .choices(vec![
            Choice::new(false, "No", "--auto keeps routing among what is logged in"),
            Choice::new(true, "Yes", format!("write [fleet.default]: {names}")),
        ])
        .when(Condition::All(vec![
            Condition::truthy("harness"),
            after_isolation(),
        ])),
    );
}

/// The keys present by name, and the backends they make: `(backend, api,
/// key variable)`.
fn detected_backends(facts: &Facts) -> Vec<(&'static str, &'static str, &'static str)> {
    let set = |name: &str| facts.variables.contains(name);
    let mut backends = Vec::new();
    if set("ANTHROPIC_API_KEY") {
        backends.push(("anthropic", "anthropic", "ANTHROPIC_API_KEY"));
    }
    if let Some(key) = ["OPENAI_API_KEY", "CODEX_API_KEY"]
        .into_iter()
        .find(|k| set(k))
    {
        backends.push(("openai", "openai", key));
    }
    if set("GEMINI_API_KEY") {
        backends.push(("gemini", "openai", "GEMINI_API_KEY"));
    }
    if set("ZAI_API_KEY") {
        backends.push(("zai", "openai", "ZAI_API_KEY"));
    }
    backends
}

/// What the generated file ends with while it has no `[models]`: where the
/// gateway is described, nothing more.
const MODELS_HINT: &str = "\n# Optional: route harness model calls through Branchyard's model gateway.\n# Its models table is described in docs/model-gateway.md.\n";

/// The Gemini model a Claude request is answered by on the `gemini`
/// backend, unless the person names another.
const CLAUDE_ON_GEMINI: &str = "gemini-3.5-flash";

/// The model gateway's backends and routes for the provider keys set here,
/// by name; offered only while the file has no backends of its own.
fn models_questions(facts: &Facts, current: &ProjectConfig, qs: &mut Vec<Question>) {
    let backends = detected_backends(facts);
    if backends.is_empty() || !current.models.backends.is_empty() {
        return;
    }
    let listed: Vec<String> = backends
        .iter()
        .map(|(name, _, key)| format!("{name} ({key})"))
        .collect();
    let anthropic = backends.iter().any(|(name, _, _)| *name == "anthropic");
    qs.push(
        Question::new(
            "models",
            Kind::Confirm,
            "Gateway",
            "Should the model gateway get backends for the provider keys set here?",
            &format!(
                "Writes [models.backends] and routes by model for {}, each key by its variable's \
                 name. Branches use the gateway when run with --model-gateway (or once [models] \
                 allow is set); nothing else changes.",
                listed.join(", ")
            ),
        )
        .optional()
        .default(false)
        .choices(vec![
            Choice::new(
                false,
                "No",
                "no [models]; docs/model-gateway.md has the block to add later",
            ),
            Choice::new(true, "Yes", format!("backends {}", listed.join(", "))),
        ]),
    );
    if !backends.iter().any(|(name, _, _)| *name == "gemini") {
        return;
    }
    qs.push(
        Question::new(
            "models.claude",
            Kind::Confirm,
            "Claude route",
            match anthropic {
                true => "Should Claude models fall back to Gemini when Anthropic cannot answer?",
                false => "Should requests for Claude models be answered by Gemini?",
            },
            "The gateway translates Claude Code's Anthropic requests into chat completions for \
             the gemini backend, so Claude Code runs on a branch without an Anthropic key or \
             login. The trade-off: the answers come from a Gemini model, not Claude, so quality \
             and tool use differ; thinking comes back unsigned; count_tokens and the rest of \
             Anthropic's API are not served (502); and this route has not been tried against \
             Gemini's endpoint itself.",
        )
        .optional()
        .default(false)
        .choices(vec![
            Choice::new(false, "No", "Claude models go to Anthropic only"),
            Choice::new(
                true,
                "Yes",
                match anthropic {
                    true => "after Anthropic fails (refused, 5xx, 429), translated for Gemini",
                    false => "translated for a Gemini model",
                },
            ),
        ])
        .when(Condition::truthy("models")),
    );
    qs.push(
        Question::new(
            "models.claude_model",
            Kind::Text,
            "Gemini model",
            "Which Gemini model should answer requests for Claude models?",
            "The route's backend_model: what the gemini backend is asked for in place of the \
             Claude model the harness names.",
        )
        .default(CLAUDE_ON_GEMINI)
        .choices(vec![Choice::new(
            CLAUDE_ON_GEMINI,
            CLAUDE_ON_GEMINI,
            "the model deepseek-harness ran on through this endpoint",
        )])
        .rule(Rule::Name { max: 64 })
        .when(Condition::truthy("models.claude")),
    );
}

/// `[models.backends]` and `[[models.routes]]` from the answers.
fn models_tables(facts: &Facts, answers: &Answers) -> (Value, Value) {
    let backends = detected_backends(facts);
    let has = |name: &str| backends.iter().any(|(n, _, _)| *n == name);
    let mut tables = serde_json::Map::new();
    for (name, api, key) in &backends {
        let (url, compat) = match *name {
            "gemini" => (Some(config::GEMINI_OPENAI_URL), Some("gemini")),
            // The GLM Coding Plan's endpoint; a standard key's is
            // https://api.z.ai/api/paas/v4 (docs/model-gateway.md, "Z.ai").
            "zai" => (Some(config::ZAI_OPENAI_URL), Some("versioned")),
            _ => (None, None),
        };
        let backend = config::ModelBackendConfig {
            api: (*api).to_owned(),
            key: Some((*key).to_owned()),
            url: url.map(str::to_owned),
            compat: compat.map(str::to_owned),
            ..config::ModelBackendConfig::default()
        };
        tables.insert(
            (*name).to_owned(),
            serde_json::to_value(backend).unwrap_or_default(),
        );
    }
    let route = |model: &str, backend: &str| config::ModelRouteConfig {
        model: model.to_owned(),
        backends: vec![backend.to_owned()],
        ..config::ModelRouteConfig::default()
    };
    let claude_model = (has("gemini") && flag(answers, "models.claude")).then(|| {
        text(answers, "models.claude_model").unwrap_or_else(|| CLAUDE_ON_GEMINI.to_owned())
    });
    let mut routes = Vec::new();
    if has("anthropic") {
        let mut claude = route("claude-*", "anthropic");
        if let Some(model) = &claude_model {
            claude.fallbacks = vec!["gemini".into()];
            claude.backend_model = Some(model.clone());
        }
        routes.push(claude);
    } else if let Some(model) = &claude_model {
        let mut claude = route("claude-*", "gemini");
        claude.backend_model = Some(model.clone());
        routes.push(claude);
    }
    if has("openai") {
        routes.push(route("gpt-*", "openai"));
        routes.push(route("codex-*", "openai"));
    }
    if has("gemini") {
        routes.push(route("gemini-*", "gemini"));
    }
    if has("zai") {
        routes.push(route("glm-*", "zai"));
    }
    (
        Value::Object(tables),
        serde_json::to_value(routes).unwrap_or_default(),
    )
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
    let setup = existing.map_or_else(
        || joined(detected.setup.clone()),
        |w| joined(w.setup.commands()),
    );
    let teardown = existing.map_or_else(
        || joined(detected.teardown.clone()),
        |w| joined(w.teardown.commands()),
    );
    let run = match existing {
        Some(w) => w.run_script(None).ok().and_then(|(_, c)| joined(c)),
        None => detected.run.clone(),
    };
    let shell_setup =
        existing.map_or_else(|| detected.shell_setup.clone(), |w| w.shell_setup.clone());
    let copy: Vec<String> = existing.map_or_else(|| detected.copy.clone(), |w| w.copy.clone());
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
                .default(value.map_or(Value::Null, Value::from))
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
    // Asked only when there is a server to start (a script the repository
    // has, or one the file already names): a default that fails on the
    // first worktree is worse than no question.
    if run.is_some() {
        qs.push(command(
            "workspace.run",
            "Run",
            "Which command starts a development server in a branch?",
            "`by workspace run BRANCH` runs it in the branch's worktree, with its own port in $BRANCHYARD_PORT.",
            run,
            "No run script",
        ));
    }
    qs.push(command(
        "workspace.teardown",
        "Teardown",
        "What should run when a branch is removed?",
        "Runs in the worktree on by rm and by merge --rm, best-effort, such as stopping the branch's containers.",
        teardown,
        "No teardown",
    ));
    qs.push(command(
        "workspace.shell_setup",
        "Shell",
        "What shell setup should run before each turn and script, such as `nvm use`?",
        "Runs with /bin/sh in the worktree before every turn's harness and every setup, run and teardown script; the variables it sets are theirs. A failure fails the turn.",
        shell_setup,
        "No shell setup",
    ));
}

pub fn plan(facts: &Facts, answers: &Answers, probe: &dyn Probe) -> Plan {
    let Target {
        path,
        existing,
        unreadable,
    } = target(facts, answers);
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
    // Logins in files, by where they are; the secrets question's own
    // answer for a name wins. One not chosen is removed only when it is
    // recorded as this question would record it.
    if answers.contains_key("logins") {
        let chosen = list(answers, "logins");
        let named = list(answers, "secrets");
        for login in &facts.logins {
            let Some(secret) = &login.secret else {
                continue;
            };
            let key = format!("secrets.{secret}");
            let source = Value::from(login_source(login));
            if chosen.contains(secret) {
                if !named.contains(secret) {
                    flat.insert(key, source);
                }
            } else if flat.get(&key) == Some(&source) {
                flat.remove(&key);
            }
        }
    }
    let fleet = match flag(answers, "fleet") {
        true => inventory_fleet(facts, answers),
        false => None,
    };
    if let Some(fleet) = &fleet {
        flat.insert(
            "fleet.default".into(),
            serde_json::to_value(fleet).unwrap_or_default(),
        );
    }
    let gateway = flag(answers, "models");
    if gateway {
        let (backends, routes) = models_tables(facts, answers);
        flat.insert("models.backends".into(), backends);
        flat.insert("models.routes".into(), routes);
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
            set!("workspace.shell_setup", get("workspace.shell_setup"));
            // What a devcontainer says beyond its scripts, applied as
            // detected for the plan to show: the variables its processes
            // get, and the file keying the environment it is built as.
            let detected = &facts.workspace;
            if before.as_ref().is_none_or(|w| w.env.is_empty()) && !detected.env.is_empty() {
                flat.insert("workspace.env".into(), json!(detected.env));
            } else if let Some(env) = before.as_ref().map(|w| &w.env).filter(|e| !e.is_empty()) {
                flat.insert("workspace.env".into(), json!(env));
            }
            let setup_kept = text(answers, "workspace.setup").is_some();
            if detected.prepare && setup_kept && before.as_ref().is_none_or(|w| !w.prepare) {
                flat.insert("workspace.prepare".into(), json!(true));
                if !detected.inputs.is_empty() {
                    let mut inputs = detected.inputs.clone();
                    // The lockfiles present key it too, as they would by
                    // default; naming inputs replaces that default.
                    for lockfile in crate::probe::ENV_INPUTS {
                        if probe.exists(lockfile) && !inputs.contains(&(*lockfile).to_owned()) {
                            inputs.push((*lockfile).to_owned());
                        }
                    }
                    flat.insert("workspace.inputs".into(), json!(inputs));
                }
            } else if let Some(w) = before.as_ref().filter(|w| w.prepare) {
                flat.insert("workspace.prepare".into(), json!(true));
                if !w.inputs.is_empty() {
                    flat.insert("workspace.inputs".into(), json!(w.inputs));
                }
                if !w.share.is_empty() {
                    flat.insert("workspace.share".into(), json!(w.share));
                }
            }
            // Other run scripts are kept; the asked one is the default.
            let mut runs = before.map(|w| w.run).unwrap_or_default();
            let name = runs
                .iter()
                .find(|(_, r)| r.default)
                .map_or_else(|| "dev".into(), |(name, _)| name.clone());
            match (
                text(answers, "workspace.run"),
                answers.contains_key("workspace.run"),
            ) {
                (Some(command), _) => {
                    runs.insert(
                        name,
                        config::RunScript {
                            command: config::Script::One(command),
                            default: true,
                        },
                    );
                }
                // Asked and cleared. A question never asked leaves them.
                (None, true) => {
                    runs.remove(&name);
                }
                (None, false) => {}
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
    // Whether the file written carries [workspace] scripts, and whether
    // they differ from the ones in the file now (a trust decision is of
    // their digest).
    let mut scripts = None;
    let body = match ProjectConfig::unflatten(&flat) {
        Ok(config) => {
            scripts = config
                .workspace
                .as_ref()
                .filter(|w| w.has_scripts())
                .map(|w| {
                    let before = existing.as_ref().and_then(|c| c.workspace.as_ref());
                    before.is_none_or(|b| b.digest() != w.digest())
                });
            let heading = match path.as_str() {
                config::PROJECT_FILE => {
                    "Branchyard project defaults, written by `by init project`."
                }
                _ => "Branchyard user defaults, written by `by init project`.",
            };
            let mut body = config::render(&config, heading);
            // The gateway is opt-in: say where it is, not what it holds.
            if config.models.is_empty() {
                body.push_str(MODELS_HINT);
            }
            body
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
    if let Some(problem) = &unreadable {
        plan.notes.push(format!(
            "{path} does not load: {}; its values are not used here, so the plan replaces the \
             file. See what `by` says of it with `by config validate`.",
            problem.problem()
        ));
    }
    if existing.is_some() {
        plan.notes.push(
            "Keys the interview does not ask about are kept; comments in the old file are not."
                .into(),
        );
    }
    if facts.platform.kvm && !facts.build.microsandbox {
        plan.notes.push(format!(
            "This host has KVM, but this build has no Microsandbox support, so harnesses run \
             locally. For a microVM per turn, rebuild with `{}` (needs libcap-ng; docs/modes.md#sandbox).",
            crate::probe::REBUILD_MICROSANDBOX
        ));
    }
    // The default harness must run here, unless branches run elsewhere.
    let runs_here = text(answers, "remote").as_deref() != Some("server")
        && text(answers, "provider").as_deref() != Some("microsandbox");
    if let Some(harness) = text(answers, "harness").filter(|_| runs_here) {
        if !facts.installed().any(|h| h.harness == harness) {
            let found: Vec<&str> = facts.installed().map(|h| h.harness.as_str()).collect();
            plan.notes.push(match found.is_empty() {
                true => format!(
                    "No harness is on PATH here, so `by run` cannot start {harness} yet: \
                     `by harnesses install {harness}` installs it."
                ),
                false => format!(
                    "{harness} is not on PATH here (found: {}), so `by run` cannot start it yet: \
                     `by harnesses install {harness}` installs it, or choose one of those.",
                    found.join(", ")
                ),
            });
        }
    }
    let mut secret_note = list(answers, "secrets");
    for login in list(answers, "logins") {
        if !secret_note.contains(&login) {
            secret_note.push(login);
        }
    }
    if !secret_note.is_empty() {
        plan.notes.push(format!(
            "Secrets by name only ({}); their values stay in their variables or files.",
            secret_note.join(", ")
        ));
    }
    if let Some(fleet) = &fleet {
        plan.notes.push(format!(
            "[fleet.default] routes every kind of task among {}; by run without --harness now \
             routes too. Remove it to route --auto by what is logged in again.",
            fleet_names(fleet)
        ));
    } else if no_fleet(facts) {
        if let Some(fleet) = inventory_fleet(facts, answers) {
            plan.notes.push(format!(
                "No [fleet]: by run --auto and by fan --auto route among the harnesses logged in \
                 when they run (now {}).",
                fleet_names(&fleet)
            ));
        }
    }
    if gateway {
        plan.notes.push(
            "The model gateway's backends hold the keys by name; a branch uses them when run \
             with --model-gateway, or once [models] allow is set."
                .into(),
        );
        plan.command(
            "by doctor",
            "Check the gateway's backends and routes, and what each route's models cost.",
        );
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
    if let Some(changed) = scripts {
        plan.notes.push(
            "[workspace] scripts run only once you trust them: review them, then run \
             `by workspace trust`. A change to them asks again."
                .into(),
        );
        // New or changed scripts are refused by the next `by run` until
        // then, so the trust leads the list; scripts already as they were
        // keep whatever decision was made.
        match changed {
            true => plan.command_first(
                "by workspace trust",
                "Your first `by run` needs this: it lets [workspace]'s setup, run and teardown scripts run for this repository.",
            ),
            false => plan.command(
                "by workspace trust",
                "Let [workspace]'s setup, run and teardown scripts run for this repository.",
            ),
        }
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
        probe.files.insert(
            "package.json".into(),
            r#"{"scripts": {"dev": "vite"}}"#.into(),
        );
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
        assert_eq!(w.shell_setup.as_deref(), Some("nvm use"));
        assert_eq!(w.notes.len(), 1, "{:?}", w.notes);
        assert!(w.notes[0].starts_with(".conductor/settings.toml was not imported:"));
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
        assert_eq!(workspace.shell_setup.as_deref(), Some("nvm use"));
        assert!(plan.valid);
    }

    /// A repository with a `package.json` holding `scripts`, and a lockfile.
    fn node(lock: &str, scripts: &str) -> FakeProbe {
        let mut probe = FakeProbe::typical();
        // Only the package: Cargo.toml would add `cargo fetch` to the setup.
        probe.files.remove("Cargo.toml");
        probe.files.insert(lock.into(), String::new());
        probe.files.insert(
            "package.json".into(),
            format!(r#"{{"name": "x", "scripts": {scripts}}}"#),
        );
        probe
    }

    #[test]
    fn a_run_script_is_proposed_only_for_a_script_the_package_has() {
        // `dev` first, then `start`, in the package manager the lockfile names.
        for (lock, scripts, run) in [
            (
                "package-lock.json",
                r#"{"dev": "vite", "start": "node ."}"#,
                "npm run dev",
            ),
            (
                "package-lock.json",
                r#"{"start": "node ."}"#,
                "npm run start",
            ),
            ("pnpm-lock.yaml", r#"{"dev": "vite"}"#, "pnpm dev"),
            ("pnpm-lock.yaml", r#"{"start": "node ."}"#, "pnpm start"),
            ("yarn.lock", r#"{"dev": "vite"}"#, "yarn dev"),
            ("bun.lock", r#"{"start": "bun ."}"#, "bun run start"),
            ("bun.lockb", r#"{"dev": "bun --hot ."}"#, "bun run dev"),
        ] {
            let facts = Facts::gather(&node(lock, scripts));
            assert_eq!(
                facts.workspace.run.as_deref(),
                Some(format!("PORT=$BRANCHYARD_PORT {run}").as_str()),
                "{lock} {scripts}"
            );
        }
        // A package with a test script and nothing to serve gets setup, and no run.
        for scripts in [r#"{"test": "node --test"}"#, "{}", "null", r#"{"dev": 1}"#] {
            let facts = Facts::gather(&node("package-lock.json", scripts));
            assert_eq!(facts.workspace.setup, ["npm ci"], "{scripts}");
            assert_eq!(facts.workspace.run, None, "{scripts}");
        }
        // No scripts at all, or not JSON: the same.
        let mut broken = node("pnpm-lock.yaml", "{}");
        broken
            .files
            .insert("package.json".into(), "{ not json".into());
        assert_eq!(Facts::gather(&broken).workspace.run, None);
        let mut empty = node("yarn.lock", "{}");
        empty.files.insert("package.json".into(), String::new());
        assert_eq!(Facts::gather(&empty).workspace.run, None);
    }

    #[test]
    fn the_run_question_is_not_asked_without_a_script_to_run() {
        let ids = |probe: &FakeProbe| -> Vec<String> {
            let facts = Facts::gather(probe);
            let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
            assert!(state.done(), "{state:?}");
            state.asked.iter().map(|q| q.id.clone()).collect()
        };
        let without = ids(&node("package-lock.json", r#"{"test": "node --test"}"#));
        assert!(
            without.contains(&"workspace.setup".to_owned()),
            "{without:?}"
        );
        assert!(
            !without.contains(&"workspace.run".to_owned()),
            "{without:?}"
        );
        // A repository with no package.json at all is asked no more than that.
        assert!(!ids(&FakeProbe::typical()).contains(&"workspace.run".to_owned()));
        let with = ids(&node("package-lock.json", r#"{"dev": "vite"}"#));
        assert!(with.contains(&"workspace.run".to_owned()), "{with:?}");

        // The plan writes setup and no run script, and parses.
        let probe = node("package-lock.json", r#"{"test": "node --test"}"#);
        let facts = Facts::gather(&probe);
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        let body = plan_body(&facts, &state.answers, &probe);
        let workspace = config::parse(&body).unwrap().workspace.unwrap();
        assert_eq!(workspace.setup.commands(), ["npm ci"]);
        assert!(workspace.run.is_empty(), "{body}");
    }

    #[test]
    fn a_run_script_the_file_already_has_is_kept_when_none_is_detected() {
        let mut probe = node("package-lock.json", r#"{"test": "node --test"}"#);
        probe.files.insert(
            "branchyard.toml".into(),
            "version = 1\n[workspace]\nsetup = \"npm ci\"\n[workspace.run.web]\ncommand = \"node server.js\"\ndefault = true\n"
                .into(),
        );
        let facts = Facts::gather(&probe);
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        assert_eq!(
            state.answers["workspace.run"],
            json!("node server.js"),
            "asked, with the file's script as the default"
        );
        // Not asked (the file's script is not a default): kept, not removed.
        let mut answers = state.answers.clone();
        answers.remove("workspace.run");
        let body = plan_body(&facts, &answers, &probe);
        let workspace = config::parse(&body).unwrap().workspace.unwrap();
        assert!(workspace.run.contains_key("web"), "{body}");
    }

    #[test]
    fn a_file_that_does_not_load_is_said_first_and_replaced() {
        let mut probe = FakeProbe::typical();
        probe.files.insert(
            "branchyard.toml".into(),
            "version = 1\nagent = \"claude\"\nbudget = 5\n[workspace]\nsetup = \"npm ci\"\n".into(),
        );
        let facts = Facts::gather(&probe);
        // Before the questions: a fact line, in the protocol's `facts` too.
        let lines = facts.lines();
        let line = lines.iter().find(|l| l.id == "config_problem").unwrap();
        assert_eq!(line.label, "branchyard.toml");
        assert!(
            line.value.starts_with(
                "does not load: line 2: unknown field `agent` (did you mean `harness`, under [defaults]?)"
            ),
            "{}",
            line.value
        );
        // Its values are not defaults, and the plan's first note says why.
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        assert_eq!(state.answers["harness"], json!("claude-code"));
        let plan = plan(&facts, &state.answers, &probe);
        assert!(
            plan.notes[0]
                .starts_with("branchyard.toml does not load: line 2: unknown field `agent`"),
            "{:?}",
            plan.notes
        );
        assert!(
            plan.notes[0].contains("its values are not used here"),
            "{:?}",
            plan.notes
        );
        assert!(
            plan.notes[0].contains("replaces the file"),
            "{:?}",
            plan.notes
        );
        assert!(
            !plan
                .notes
                .iter()
                .any(|n| n.contains("Keys the interview does not ask about are kept")),
            "nothing of the old file is kept: {:?}",
            plan.notes
        );
        assert!(plan.valid || plan.files[0].overwrites);
        // A file that loads says nothing of the kind.
        probe
            .files
            .insert("branchyard.toml".into(), "version = 1\n".into());
        let facts = Facts::gather(&probe);
        assert!(!facts.lines().iter().any(|l| l.id == "config_problem"));
    }

    #[test]
    fn a_user_file_that_does_not_load_is_said_too() {
        let mut probe = FakeProbe::typical();
        let facts = Facts::gather(&probe);
        probe.files.insert(
            facts.user_config_path.clone(),
            "[defaults]\nbudget = 3\n".into(),
        );
        let facts = Facts::gather(&probe);
        let line = facts
            .lines()
            .into_iter()
            .find(|l| l.id == "config_problem")
            .unwrap();
        assert_eq!(line.label, facts.user_config_path);
        assert!(
            line.value
                .contains("line 2: unknown field `budget` (did you mean `budget_usd`?)"),
            "{}",
            line.value
        );
        let user = answers(json!({"scope": "user"}));
        let state = resolve(&|a| questions(&facts, a), &user, true);
        let plan = plan(&facts, &state.answers, &probe);
        assert!(
            plan.notes[0].starts_with(&format!("{} does not load", facts.user_config_path)),
            "{:?}",
            plan.notes
        );
    }

    #[test]
    fn new_workspace_scripts_lead_the_next_steps_with_the_trust_they_need() {
        let mut probe = node("package-lock.json", r#"{"dev": "vite"}"#);
        probe.files.insert(".env".into(), String::new());
        let facts = Facts::gather(&probe);
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        let first = plan(&facts, &state.answers, &probe);
        let commands: Vec<&str> = first.commands.iter().map(|c| c.command.as_str()).collect();
        assert_eq!(commands[0], "by workspace trust", "{commands:?}");
        assert_eq!(
            commands
                .iter()
                .filter(|c| **c == "by workspace trust")
                .count(),
            1
        );
        assert!(
            first.commands[0]
                .why
                .contains("Your first `by run` needs this"),
            "{:?}",
            first.commands[0]
        );
        assert!(commands.contains(&"by config validate"), "{commands:?}");

        // The same scripts in the file now: its decision stands, nothing leads.
        let written = plan_body(&facts, &state.answers, &probe);
        probe.files.insert("branchyard.toml".into(), written);
        let facts = Facts::gather(&probe);
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        let again = plan(&facts, &state.answers, &probe);
        assert_eq!(again.commands[0].command, "by config validate");
        assert!(again
            .commands
            .iter()
            .any(|c| c.command == "by workspace trust"));

        // Copying files runs nothing, so there is nothing to trust.
        let mut copy_only = FakeProbe::typical();
        copy_only.files.remove("Cargo.toml");
        copy_only.files.insert(".env".into(), String::new());
        let facts = Facts::gather(&copy_only);
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        let plan = plan(&facts, &state.answers, &copy_only);
        assert!(
            !plan
                .commands
                .iter()
                .any(|c| c.command == "by workspace trust"),
            "{:?}",
            plan.commands
        );
        // Declining the workspace writes nothing to trust either.
        let mut declined = state.answers.clone();
        declined.insert("workspace".into(), json!(false));
        assert!(!super::plan(&facts, &declined, &copy_only)
            .commands
            .iter()
            .any(|c| c.command == "by workspace trust"));
    }

    #[test]
    fn a_default_harness_that_is_not_on_path_is_warned_about_in_the_plan() {
        let mut probe = FakeProbe::typical();
        for h in &mut probe.harnesses {
            h.installed = h.harness == "codex";
        }
        let facts = Facts::gather(&probe);
        let note = |answers: &Answers| plan(&facts, answers, &probe).notes.join("\n");
        let claude = answers(json!({"harness": "claude-code"}));
        let state = resolve(&|a| questions(&facts, a), &claude, true);
        let text = note(&state.answers);
        assert!(
            text.contains("claude-code is not on PATH here (found: codex)")
                && text.contains("`by harnesses install claude-code`"),
            "{text}"
        );
        // One that is installed, a server that runs it elsewhere: no warning.
        let codex = resolve(
            &|a| questions(&facts, a),
            &answers(json!({"harness": "codex"})),
            true,
        );
        assert!(!note(&codex.answers).contains("not on PATH"));
        let server = answers(json!({"harness": "claude-code", "remote": "server"}));
        let remote = resolve(&|a| questions(&facts, a), &server, true);
        assert!(!note(&remote.answers).contains("not on PATH"));
        // Nothing installed at all.
        for h in &mut probe.harnesses {
            h.installed = false;
        }
        let facts = Facts::gather(&probe);
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        let text = plan(&facts, &state.answers, &probe).notes.join("\n");
        assert!(
            text.contains("No harness is on PATH here, so `by run` cannot start claude-code yet"),
            "{text}"
        );
    }

    #[test]
    fn a_devcontainer_is_imported_as_a_prepared_workspace_with_its_variables() {
        let mut probe = FakeProbe::typical();
        for file in ["package.json", "pnpm-lock.yaml"] {
            probe.files.insert(file.into(), String::new());
        }
        probe.files.insert(
            ".devcontainer/devcontainer.json".into(),
            r#"{
  // the team's image
  "image": "mcr.microsoft.com/devcontainers/typescript-node:1-20",
  "remoteEnv": { "CI": "true", "CACHE": "${containerWorkspaceFolder}/.cache" },
  "postCreateCommand": "pnpm install --frozen-lockfile",
  "postStartCommand": ["pnpm", "dev", "--port", "${localEnv:PORT:3000}"],
  "forwardPorts": [3000],
}"#
            .into(),
        );
        let facts = Facts::gather(&probe);
        let w = &facts.workspace;
        assert_eq!(w.setup, ["pnpm install --frozen-lockfile"]);
        assert_eq!(w.run.as_deref(), Some("pnpm dev --port \"${PORT:-3000}\""));
        assert_eq!(w.env["CACHE"], "${BRANCHYARD_WORKTREE}/.cache");
        assert!(w.prepare);
        assert_eq!(w.inputs, [".devcontainer/devcontainer.json"]);
        assert!(
            w.notes
                .iter()
                .any(|n| n.contains("needs a container runtime")),
            "{:?}",
            w.notes
        );
        assert!(w
            .notes
            .iter()
            .any(|n| n.contains("forwardPorts is not imported")));
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        let plan = plan(&facts, &state.answers, &probe);
        assert!(plan.valid);
        let body = plan_body(&facts, &state.answers, &probe);
        let workspace = config::parse(&body).unwrap().workspace.unwrap();
        assert_eq!(
            workspace.setup.commands(),
            ["pnpm install --frozen-lockfile"]
        );
        assert_eq!(workspace.env["CI"], "true");
        assert_eq!(workspace.env["CACHE"], "${BRANCHYARD_WORKTREE}/.cache");
        assert!(workspace.prepare, "{body}");
        // The file keys the environment, with the lockfiles present.
        assert_eq!(
            workspace.inputs,
            [
                ".devcontainer/devcontainer.json",
                "package.json",
                "pnpm-lock.yaml"
            ]
        );
        assert!(body.contains("[workspace.env]"), "{body}");
    }

    fn plan_body(facts: &Facts, answers: &Answers, probe: &FakeProbe) -> String {
        plan(facts, answers, probe).files[0]
            .content
            .clone()
            .unwrap()
    }

    fn login(state: &str, evidence: &str) -> Option<crate::probe::HarnessLogin> {
        Some(crate::probe::HarnessLogin {
            state: state.into(),
            evidence: evidence.into(),
            detail: format!("{state} ({evidence})"),
            label: String::new(),
        })
    }

    /// Today's machine: Codex logged in by device code (its auth file has a
    /// token), Antigravity and Gemini CLI by GEMINI_API_KEY with
    /// Antigravity's settings naming Gemini, Claude Code logged out;
    /// GEMINI_API_KEY and CODEX_API_KEY set.
    fn machine() -> FakeProbe {
        let mut probe = FakeProbe::typical();
        probe.variables = [
            "GEMINI_API_KEY".to_owned(),
            "CODEX_API_KEY".to_owned(),
            "ZAI_API_KEY".to_owned(),
        ]
        .into();
        for h in &mut probe.harnesses {
            (h.installed, h.login) = match h.harness.as_str() {
                "claude-code" => (true, login("logged_out", "likely")),
                "codex" => (true, login("logged_in", "verified")),
                "antigravity" | "gemini-cli" => (true, login("logged_in", "likely")),
                _ => (false, None),
            };
        }
        probe.files.insert(
            "~/.codex/auth.json".into(),
            r#"{"auth_mode": "chatgpt", "tokens": {"refresh_token": "rt-SECRET-VALUE-1"}}"#.into(),
        );
        probe.files.insert(
            "~/.gemini/antigravity-cli/settings.json".into(),
            r#"{"modelProvider": "gemini"}"#.into(),
        );
        probe
    }

    fn answers(value: Value) -> BTreeMap<String, Value> {
        serde_json::from_value(value).unwrap()
    }

    fn config_of(facts: &Facts, answers: &Answers, probe: &FakeProbe) -> ProjectConfig {
        let body = plan_body(facts, answers, probe);
        assert!(!body.contains("rt-SECRET"), "{body}");
        config::parse(&body).unwrap_or_else(|e| panic!("{e}\n{body}"))
    }

    #[test]
    fn harness_logins_in_files_are_found_and_recorded_by_where_they_are() {
        let probe = machine();
        let facts = Facts::gather(&probe);
        assert_eq!(facts.logins.len(), 2, "{:?}", facts.logins);
        let codex = &facts.logins[0];
        assert_eq!(
            (
                codex.file.as_str(),
                codex.secret.as_deref(),
                codex.label.as_deref()
            ),
            (
                "~/.codex/auth.json",
                Some("CODEX_AUTH"),
                Some("auth_mode chatgpt")
            )
        );
        assert_eq!(facts.logins[1].secret, None);
        let lines = serde_json::to_string(&facts.lines()).unwrap();
        assert!(!lines.contains("rt-SECRET"), "{lines}");
        assert!(lines.contains("codex ~/.codex/auth.json (auth_mode chatgpt), as CODEX_AUTH"));
        assert!(lines.contains("antigravity ~/.gemini/antigravity-cli/settings.json (modelProvider gemini), not provisioned"));
        assert!(lines.contains("claude-code 2.1.283 (Claude Code) (logged out, likely)"));
        // A label that could be a credential never comes back.
        assert_eq!(
            crate::probe::label_in(r#"{"k": "sk-ant-api03-AbC/xyz+=="}"#, "k"),
            None
        );
        assert_eq!(crate::probe::label_in(r#"{"k": {"nested": 1}}"#, "k"), None);

        // Logged out Claude Code is not suggested; logged in Codex is.
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        assert_eq!(state.answers["harness"], json!("codex"));
        // Not isolated: offered, not chosen, and nothing recorded.
        assert_eq!(state.answers["logins"], json!([]));
        let config = config_of(&facts, &state.answers, &probe);
        assert!(config.secrets.is_empty(), "{:?}", config.secrets);
        // Isolated: suggested for the installed harness, by where it is.
        let isolated = resolve(
            &|a| questions(&facts, a),
            &answers(json!({"isolated": true, "secrets": []})),
            true,
        );
        assert_eq!(isolated.answers["logins"], json!(["CODEX_AUTH"]));
        let config = config_of(&facts, &isolated.answers, &probe);
        assert_eq!(config.secrets["CODEX_AUTH"], "@~/.codex/auth.json");
        // Chosen by label without isolation, it is recorded too; dropped
        // again, the entry this question wrote goes.
        let chosen = resolve(
            &|a| questions(&facts, a),
            &answers(json!({"logins": "Codex login"})),
            true,
        );
        let mut probe_with = probe.clone();
        let written = plan_body(&facts, &chosen.answers, &probe);
        assert!(
            written.contains("CODEX_AUTH = \"@~/.codex/auth.json\""),
            "{written}"
        );
        probe_with
            .files
            .insert(config::PROJECT_FILE.into(), written.clone());
        let again = Facts::gather(&probe_with);
        let kept = resolve(&|a| questions(&again, a), &BTreeMap::new(), true);
        assert_eq!(kept.answers["logins"], json!(["CODEX_AUTH"]));
        let dropped = resolve(
            &|a| questions(&again, a),
            &answers(json!({"logins": []})),
            true,
        );
        let config = config_of(&again, &dropped.answers, &probe_with);
        assert!(!config.secrets.contains_key("CODEX_AUTH"));
    }

    #[test]
    fn the_inventory_default_can_be_written_as_the_fleet_default() {
        let probe = machine();
        let facts = Facts::gather(&probe);
        // Offered, off by default: --auto keeps routing by the inventory.
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        assert_eq!(state.answers["fleet"], json!(false));
        let plan = plan(&facts, &state.answers, &probe);
        assert!(
            plan.notes
                .iter()
                .any(|n| n.contains("route among the harnesses logged in when they run (now codex, antigravity, gemini-cli)")),
            "{:?}",
            plan.notes
        );
        assert!(config_of(&facts, &state.answers, &probe).fleet.is_empty());
        // Written: the logged-in harnesses, the default harness first, the
        // first that routes permission requests as the judge.
        let chosen = resolve(
            &|a| questions(&facts, a),
            &answers(json!({"fleet": "yes", "harness": "gemini-cli"})),
            true,
        );
        let fleet = config_of(&facts, &chosen.answers, &probe).fleet["default"].clone();
        let names: Vec<&str> = fleet
            .candidates
            .iter()
            .map(|c| c.harness.as_str())
            .collect();
        assert_eq!(names, ["gemini-cli", "codex", "antigravity"]);
        assert!(fleet
            .candidates
            .iter()
            .all(|c| c.model.is_none() && c.effort.is_none()));
        assert_eq!(fleet.judge.unwrap().harness, "gemini-cli");
        assert_eq!((fleet.attempts, fleet.failover), (None, None));
        // Isolated, Codex takes effort medium; Antigravity is never judge.
        let isolated = resolve(
            &|a| questions(&facts, a),
            &answers(
                json!({"fleet": true, "isolated": true, "harness": "antigravity", "secrets": []}),
            ),
            true,
        );
        let fleet = config_of(&facts, &isolated.answers, &probe).fleet["default"].clone();
        assert_eq!(fleet.candidates[1].effort.as_deref(), Some("medium"));
        assert_eq!(fleet.candidates[0].harness, "antigravity");
        let judge = fleet.judge.unwrap();
        assert_eq!(
            (judge.harness.as_str(), judge.effort.as_deref()),
            ("codex", Some("medium"))
        );
        // A [fleet] in either file is the configuration's: not asked.
        let mut configured = probe.clone();
        configured.files.insert(
            config::PROJECT_FILE.into(),
            "[fleet.docs]\ncandidates = [{ harness = \"codex\" }]\n".into(),
        );
        let facts = Facts::gather(&configured);
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        assert!(!state.answers.contains_key("fleet"), "{:?}", state.answers);
        // Nothing logged in: not asked either.
        let facts = Facts::gather(&FakeProbe::typical());
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        assert!(!state.answers.contains_key("fleet"));
    }

    #[test]
    fn the_gateway_gets_backends_for_the_keys_set_here() {
        let probe = machine();
        let facts = Facts::gather(&probe);
        // With keys set, --defaults still leaves the gateway out: it is
        // asked, and opt-in. The file says where to read about it.
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        assert_eq!(state.answers["models"], json!(false));
        assert!(state
            .answers
            .get("models.claude")
            .is_none_or(Value::is_null));
        let body = plan_body(&facts, &state.answers, &probe);
        assert!(!body.contains("[models"), "{body}");
        assert!(body.ends_with(MODELS_HINT), "{body}");
        assert!(body.contains("docs/model-gateway.md"), "{body}");
        assert!(config_of(&facts, &state.answers, &probe).models.is_empty());

        // Chosen, it writes backends and routes for the keys; Claude on
        // Gemini stays off.
        let state = resolve(
            &|a| questions(&facts, a),
            &answers(json!({"models": true})),
            true,
        );
        assert_eq!(state.answers["models.claude"], json!(false));
        let body = plan_body(&facts, &state.answers, &probe);
        assert!(
            !body.contains("Optional: route harness model calls"),
            "{body}"
        );
        let models = config_of(&facts, &state.answers, &probe).models;
        assert_eq!(
            models.backends.keys().collect::<Vec<_>>(),
            ["gemini", "openai", "zai"]
        );
        let zai = &models.backends["zai"];
        assert_eq!(zai.api, "openai");
        assert_eq!(zai.url.as_deref(), Some(config::ZAI_OPENAI_URL));
        assert_eq!(zai.key.as_deref(), Some("ZAI_API_KEY"));
        assert_eq!(zai.compat.as_deref(), Some("versioned"));
        let gemini = &models.backends["gemini"];
        assert_eq!(gemini.api, "openai");
        assert_eq!(gemini.url.as_deref(), Some(config::GEMINI_OPENAI_URL));
        assert_eq!(gemini.key.as_deref(), Some("GEMINI_API_KEY"));
        assert_eq!(gemini.compat.as_deref(), Some("gemini"));
        assert_eq!(
            models.backends["openai"].key.as_deref(),
            Some("CODEX_API_KEY")
        );
        assert_eq!(models.backends["openai"].url, None);
        let routes: Vec<(&str, &str)> = models
            .routes
            .iter()
            .map(|r| (r.model.as_str(), r.backends[0].as_str()))
            .collect();
        assert_eq!(
            routes,
            [
                ("gpt-*", "openai"),
                ("codex-*", "openai"),
                ("gemini-*", "gemini"),
                ("glm-*", "zai")
            ]
        );
        assert!(models.allow.is_none(), "branches are not put on it");

        // Claude models on Gemini, translated, with a backend model.
        let claude = resolve(
            &|a| questions(&facts, a),
            &answers(json!({"models": true, "models.claude": "yes"})),
            true,
        );
        let models = config_of(&facts, &claude.answers, &probe).models;
        let route = &models.routes[0];
        assert_eq!(
            (
                route.model.as_str(),
                route.backends.as_slice(),
                route.backend_model.as_deref()
            ),
            (
                "claude-*",
                &["gemini".to_owned()][..],
                Some("gemini-3.5-flash")
            )
        );
        // With an Anthropic key, Gemini is the fallback after Anthropic.
        let mut both = machine();
        both.variables.insert("ANTHROPIC_API_KEY".into());
        let facts = Facts::gather(&both);
        let state = resolve(
            &|a| questions(&facts, a),
            &answers(json!({
                "models": true,
                "models.claude": true,
                "models.claude_model": "gemini-2.5-pro"
            })),
            true,
        );
        let models = config_of(&facts, &state.answers, &both).models;
        let claude = &models.routes[0];
        assert_eq!(claude.backends, ["anthropic"]);
        assert_eq!(claude.fallbacks, ["gemini"]);
        assert_eq!(claude.backend_model.as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(
            models.backends["anthropic"].key.as_deref(),
            Some("ANTHROPIC_API_KEY")
        );
        // Skipped, nothing is written.
        let skipped = resolve(
            &|a| questions(&facts, a),
            &answers(json!({"models": "skip"})),
            true,
        );
        assert!(config_of(&facts, &skipped.answers, &both).models.is_empty());

        // No key: no question, and --defaults keeps the gateway off.
        let mut keyless = machine();
        keyless.variables.clear();
        let facts = Facts::gather(&keyless);
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        assert!(!state.answers.contains_key("models"));
        assert!(config_of(&facts, &state.answers, &keyless)
            .models
            .is_empty());
        // Backends of its own: the file is the configuration's.
        let mut configured = machine();
        configured.files.insert(
            config::PROJECT_FILE.into(),
            "[models.backends.local]\napi = \"openai\"\nurl = \"http://127.0.0.1:8000\"\n".into(),
        );
        let facts = Facts::gather(&configured);
        let state = resolve(&|a| questions(&facts, a), &BTreeMap::new(), true);
        assert!(!state.answers.contains_key("models"));
        let models = config_of(&facts, &state.answers, &configured).models;
        assert_eq!(models.backends.keys().collect::<Vec<_>>(), ["local"]);
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
