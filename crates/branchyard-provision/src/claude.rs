// Derived from Scion (https://github.com/GoogleCloudPlatform/scion) at
// d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338: harnesses/claude/provision.py,
// the model aliases of harnesses/claude/config.yaml, and the model tests of
// harnesses/claude/provision_test.py. Copyright 2026 Google LLC. Licensed
// under the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to a sans-IO planner.
// An API key is not set in the environment with its fingerprint approved
// in `.claude.json`, as Scion does: it is written to a 0600 file in the
// private home that `apiKeyHelper` in `~/.claude/settings.json` prints, so
// the harness's tool commands do not inherit it. The workspace is marked
// trusted without dropping other projects; a credentials file is written
// from the CLAUDE_AUTH secret rather than mounted; no model is set unless
// one is asked for (Scion defaults to "opus"); telemetry goes to the
// endpoint the task names, without Scion's cloud-provider checks, which
// belong to its own collector; MCP servers and instructions stay on the
// driver's session channel (`--mcp-config` with a 0600 file, plugin or
// system prompt, or ACP), so Scion's `.claude.json` MCP merge and
// CLAUDE.md projection are not used; the version probe (`claude
// --version`) is dropped, since planning runs no processes. Scion's launch
// flags (`--dangerously-skip-permissions`) are not ported: Branchyard's
// drivers route every permission request.

//! Claude Code: `claude-code-stream-json` and `claude-code-acp`.
//!
//! An API key reaches Claude Code through `apiKeyHelper`, a command in
//! `~/.claude/settings.json` whose output is the key, here `cat` of a 0600
//! file in the private home. Claude Code 2.1.283, and the 2.1.280 that
//! claude-agent-acp 0.81.2 runs (it loads user settings), use a helper from
//! user settings without the workspace trust or key approval an
//! environment key needs, and the key never enters the environment its
//! Bash tool and MCP servers inherit. With a helper,
//! `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST` is not set: 2.1.283 ignores
//! `apiKeyHelper` when it is. An OAuth token and the Vertex
//! variables have no such setting and stay in the environment, visible to
//! tool commands: Claude Code does not filter its Bash tool's environment
//! unless `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB` is on, which on Linux needs
//! bubblewrap and is not set here.
//!
//! With the stream-json driver, MCP servers go in a 0600 file passed to
//! `--mcp-config` by path, never on the command line.

use serde_json::json;

use crate::auth::{Method, Spec};
use crate::edit::{path, Edit, JsonEdit};
use crate::{
    needs_private_home, pass_session, unsupported, unused, Context, Credential, EnvVar,
    McpConfigFile, Plan, Protocol, Provisioner, Refused, Via,
};

const CLAUDE_JSON: &str = ".claude.json";
pub(crate) const CREDENTIALS: &str = ".claude/.credentials.json";
const SETTINGS: &str = ".claude/settings.json";
/// The API key `apiKeyHelper` prints.
pub(crate) const API_KEY_FILE: &str = ".branchyard/credentials/anthropic-api-key";
/// The stream-json driver's MCP configuration, in a private home.
pub(crate) const MCP_CONFIG: &str = ".branchyard/claude-mcp.json";

/// Scion's `model_aliases` for Claude.
pub const MODEL_ALIASES: &[(&str, &str)] = &[
    ("small", "haiku"),
    ("medium", "claude-sonnet-5"),
    ("large", "claude-opus-5-5"),
    ("extra-large", "claude-fable-5-1"),
];

/// Shorthand spellings, as Scion's `config.NormalizeModelAlias` accepts.
pub const MODEL_ALIAS_SHORTHAND: &[(&str, &str)] = &[
    ("s", "small"),
    ("m", "medium"),
    ("l", "large"),
    ("xl", "extra-large"),
];

/// The size aliases that resolve.
pub const KNOWN_MODEL_ALIASES: &[&str] = &["small", "medium", "large", "extra-large"];

pub const AUTH: Spec = Spec {
    harness: "claude-code",
    methods: &[
        Method::env("api-key", &["ANTHROPIC_API_KEY"], "set ANTHROPIC_API_KEY"),
        Method::env(
            "oauth-token",
            &["CLAUDE_CODE_OAUTH_TOKEN"],
            "set CLAUDE_CODE_OAUTH_TOKEN (generate with `claude setup-token`)",
        ),
        Method::file(
            "auth-file",
            "CLAUDE_AUTH",
            "provide CLAUDE_AUTH, the content of ~/.claude/.credentials.json",
        ),
        Method::env_all(
            "vertex-ai",
            &["GOOGLE_CLOUD_PROJECT"],
            &["GOOGLE_CLOUD_LOCATION", "GOOGLE_CLOUD_REGION"],
            "provide GOOGLE_CLOUD_PROJECT + GOOGLE_CLOUD_LOCATION/GOOGLE_CLOUD_REGION \
             (with ADC available to the harness) for Vertex AI",
        ),
    ],
};

/// Lower-case, and expand `s`, `m`, `l` and `xl`.
pub fn normalize_model_alias(raw: &str) -> String {
    let lowered = raw.trim().to_lowercase();
    MODEL_ALIAS_SHORTHAND
        .iter()
        .find(|(short, _)| *short == lowered)
        .map_or(lowered, |(_, full)| (*full).to_owned())
}

/// A size alias through `aliases`; anything else, normalized, unchanged.
pub fn resolve_model_alias(raw: &str, aliases: &[(&str, &str)]) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let normalized = normalize_model_alias(raw);
    if !KNOWN_MODEL_ALIASES.contains(&normalized.as_str()) {
        return normalized;
    }
    aliases
        .iter()
        .find(|(alias, _)| *alias == normalized)
        .map_or(normalized.clone(), |(_, model)| (*model).to_owned())
}

/// Where Claude Code reads its managed settings on Linux and WSL, and where
/// a sandboxed turn's are written; see `docs/provisioning.md#managed-settings`.
pub const MANAGED_SETTINGS: &str = "/etc/claude-code/managed-settings.json";

/// Claude Code's built-in tool names, as its tools reference spells them in
/// permission rules, and the names it had before that reference
/// (`MultiEdit`, `BashOutput`, `KillBash`, `KillShell`). A denied name not in
/// this list is another harness's, such as `shell` or `write_file`, and is
/// not written: Claude Code would match nothing with it.
const CLAUDE_CODE_TOOLS: &[&str] = &[
    "Agent",
    "Artifact",
    "AskUserQuestion",
    "Bash",
    "CronCreate",
    "CronDelete",
    "CronList",
    "Edit",
    "EndConversation",
    "EnterPlanMode",
    "EnterWorktree",
    "ExitPlanMode",
    "ExitWorktree",
    "Glob",
    "Grep",
    "LSP",
    "ListAgents",
    "ListMcpResourcesTool",
    "Monitor",
    "NotebookEdit",
    "PowerShell",
    "PushNotification",
    "ReadMcpResourceTool",
    "Read",
    "RemoteTrigger",
    "ReportFindings",
    "ScheduleWakeup",
    "SendFeedback",
    "SendMessage",
    "SendUserFile",
    "ShareOnboardingGuide",
    "Skill",
    "SubagentHandback",
    "TaskCreate",
    "TaskGet",
    "TaskList",
    "TaskOutput",
    "TaskStop",
    "TaskUpdate",
    "TodoWrite",
    "ToolSearch",
    "WaitForMcpServers",
    "WebFetch",
    "WebSearch",
    "Workflow",
    "Write",
    "MultiEdit",
    "BashOutput",
    "KillBash",
    "KillShell",
];

/// The variables [`plan`] sets to turn Claude Code's telemetry export off.
/// Pinned in managed settings too, so a setting in the home cannot turn it
/// back on.
const TELEMETRY_OFF: &[(&str, &str)] = &[
    ("CLAUDE_CODE_ENABLE_TELEMETRY", "0"),
    ("OTEL_METRICS_EXPORTER", "none"),
    ("OTEL_LOGS_EXPORTER", "none"),
    ("OTEL_TRACES_EXPORTER", "none"),
];

/// A sandboxed Claude Code turn's managed settings, and what they say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedSettings {
    /// The tool names denied, sorted: the policy's denials that Claude Code
    /// has a name for, and `mcp__` names.
    pub deny: Vec<String>,
    /// Whether the telemetry variables are pinned off.
    pub telemetry_off: bool,
    /// The file's content, with sorted keys and a trailing newline.
    pub json: String,
}

/// The managed settings for a sandboxed Claude Code turn, from the tool
/// names its policy denies (`denied`, names of any harness): the Claude Code
/// ones are denied, bypass mode is disabled, and with `telemetry_off` the
/// telemetry variables are pinned. Nothing else goes in the file. A policy
/// with no denials gets no `deny` list; a turn with telemetry on or unset
/// gets no `env` block, since provisioning sets no telemetry variable then.
pub fn managed_settings(denied: &[&str], telemetry_off: bool) -> ManagedSettings {
    let mut deny: Vec<String> = denied
        .iter()
        .copied()
        .filter(|name| CLAUDE_CODE_TOOLS.contains(name) || name.starts_with("mcp__"))
        .map(str::to_owned)
        .collect();
    deny.sort();
    deny.dedup();
    let mut permissions = json!({ "disableBypassPermissionsMode": "disable" });
    if !deny.is_empty() {
        permissions["deny"] = json!(deny);
    }
    let mut document = json!({ "permissions": permissions });
    if telemetry_off {
        let env: serde_json::Map<String, serde_json::Value> = TELEMETRY_OFF
            .iter()
            .map(|(name, value)| ((*name).to_owned(), json!(value)))
            .collect();
        document["env"] = serde_json::Value::Object(env);
    }
    ManagedSettings {
        deny,
        telemetry_off,
        json: crate::json_text(&document),
    }
}

pub(crate) struct Claude;

impl Provisioner for Claude {
    fn harness(&self) -> &'static str {
        "claude-code"
    }

    fn scion(&self) -> Option<&'static str> {
        Some("claude")
    }

    fn plan(&self, context: &Context) -> Result<Plan, Refused> {
        if context.effort.is_some() {
            return Err(unsupported(
                context,
                "reasoning effort",
                "Scion's Claude provisioner has no setting for it",
            ));
        }
        let mut plan = Plan::default();
        pass_session(context, &mut plan);
        let given: Vec<&str> = context.secrets.iter().map(|s| s.name.as_str()).collect();
        let resolved = AUTH.select(&given, context.auth.as_deref())?;
        let secret = |name: &str| context.secret(name).unwrap_or_default().to_owned();
        if let Some(resolved) = resolved {
            plan.auth = Some(resolved.method.to_owned());
            match resolved.method {
                "api-key" => {
                    needs_private_home(context, "the API key file")?;
                    plan.edit(API_KEY_FILE, true, Edit::Put(secret("ANTHROPIC_API_KEY")));
                    plan.edit(
                        SETTINGS,
                        true,
                        Edit::Json {
                            edits: vec![JsonEdit::Set(
                                path(&["apiKeyHelper"]),
                                json!(helper(context)),
                            )],
                            comment_lines: false,
                        },
                    );
                    plan.deliver(
                        "ANTHROPIC_API_KEY",
                        Via::Helper {
                            path: API_KEY_FILE.into(),
                            setting: format!("apiKeyHelper in {SETTINGS}"),
                        },
                        false,
                    );
                }
                // Claude Code has no file or helper for a token from
                // `claude setup-token`: `.credentials.json` holds a whole
                // claude.ai login (CLAUDE_AUTH).
                "oauth-token" => plan.secret_env(
                    "CLAUDE_CODE_OAUTH_TOKEN",
                    "CLAUDE_CODE_OAUTH_TOKEN",
                    &secret("CLAUDE_CODE_OAUTH_TOKEN"),
                    true,
                ),
                "auth-file" => {
                    needs_private_home(context, "the credentials file")?;
                    let content = secret("CLAUDE_AUTH");
                    if serde_json::from_str::<serde_json::Value>(&content).is_err() {
                        return Err(Refused("the CLAUDE_AUTH secret is not valid JSON".into()));
                    }
                    plan.edit(CREDENTIALS, true, Edit::Put(content));
                    plan.deliver(
                        "CLAUDE_AUTH",
                        Via::File {
                            path: CREDENTIALS.into(),
                        },
                        false,
                    );
                }
                "vertex-ai" => {
                    let region = resolved.env_key.unwrap_or("GOOGLE_CLOUD_REGION");
                    plan.set_env(EnvVar::plain("CLAUDE_CODE_USE_VERTEX", "1"));
                    plan.secret_env(
                        "GOOGLE_CLOUD_PROJECT",
                        "ANTHROPIC_VERTEX_PROJECT_ID",
                        &secret("GOOGLE_CLOUD_PROJECT"),
                        true,
                    );
                    plan.secret_env(region, "CLOUD_ML_REGION", &secret(region), true);
                }
                _ => unreachable!("every method of AUTH is handled"),
            }
            // Suppress model upgrade and fallback dialogs: the provider is
            // chosen here. Not with an API key: Claude Code 2.1.283 then
            // ignores `apiKeyHelper` and reports it is not logged in.
            if resolved.method != "api-key" {
                plan.set_env(EnvVar::plain("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1"));
            }
            if context.private_home {
                trust_workspace(context, &mut plan);
            }
        }
        // An API key an earlier turn wrote, and its helper, go when this
        // turn authenticates another way (or not at all).
        let had_key = context
            .installed
            .credentials
            .iter()
            .any(|c| c.path() == API_KEY_FILE);
        if had_key && plan.auth.as_deref() != Some("api-key") {
            plan.edit(API_KEY_FILE, true, Edit::Remove);
            plan.edit(
                SETTINGS,
                false,
                Edit::Json {
                    edits: vec![JsonEdit::RemoveIf(
                        path(&["apiKeyHelper"]),
                        json!(helper(context)),
                    )],
                    comment_lines: false,
                },
            );
        }
        mcp_config(context, &mut plan);
        plan.unused_secrets = unused(context, &AUTH.names());
        if let Some(telemetry) = &context.telemetry {
            let on = telemetry.enabled;
            let exporter = if on { "otlp" } else { "none" };
            let endpoint = telemetry.endpoint();
            for (name, value) in [
                ("CLAUDE_CODE_ENABLE_TELEMETRY", if on { "1" } else { "0" }),
                ("OTEL_METRICS_EXPORTER", exporter),
                ("OTEL_LOGS_EXPORTER", exporter),
                ("OTEL_TRACES_EXPORTER", "none"),
                ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint),
                ("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", endpoint),
                ("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", endpoint),
                ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
            ] {
                plan.set_env(EnvVar::plain(name, value));
            }
        }
        if let Some(model) = &context.model {
            let model = resolve_model_alias(model, MODEL_ALIASES);
            // The stream-json driver passes it as `--model` too, which
            // its own settings cannot override.
            if context.protocol == Protocol::ClaudeStreamJson {
                plan.session.model = Some(model.clone());
            }
            plan.set_env(EnvVar::plain("ANTHROPIC_MODEL", model));
        }
        Ok(plan)
    }
}

/// The `apiKeyHelper` command: print the key file, as the harness sees it.
fn helper(context: &Context) -> String {
    let file = context.home_path(API_KEY_FILE);
    format!("cat '{}'", file.replace('\'', r"'\''"))
}

/// The stream-json driver's MCP servers as a 0600 file: in a private home,
/// written by the plan (and removed when a later turn has none); otherwise
/// placed by the caller for the turn. Other protocols pass them over stdin.
fn mcp_config(context: &Context, plan: &mut Plan) {
    if context.protocol != Protocol::ClaudeStreamJson {
        return;
    }
    let recorded = context
        .installed
        .credentials
        .iter()
        .any(|c| matches!(c, Credential::File { path } if path == MCP_CONFIG));
    if context.mcp_servers.is_empty() && context.remote_mcp_servers.is_empty() {
        if recorded {
            plan.edit(MCP_CONFIG, true, Edit::Remove);
        }
        return;
    }
    let content = branchyard_harness::claude_code::mcp_config(
        &context.mcp_servers,
        &context.remote_mcp_servers,
    );
    let path = match context.private_home {
        true => {
            plan.edit(MCP_CONFIG, true, Edit::Put(content.clone()));
            Some(context.home_path(MCP_CONFIG))
        }
        false => None,
    };
    plan.session.mcp_config = Some(McpConfigFile { path, content });
}

/// Mark the workspace trusted in `.claude.json`, keeping other projects.
fn trust_workspace(context: &Context, plan: &mut Plan) {
    let project = |key: &str| path(&["projects", &context.workspace, key]);
    plan.edit(
        CLAUDE_JSON,
        false,
        Edit::Json {
            edits: vec![
                JsonEdit::Set(project("hasTrustDialogAccepted"), json!(true)),
                JsonEdit::Default(project("projectOnboardingSeenCount"), json!(1)),
            ],
            comment_lines: false,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // From Scion's claude/provision_test.py, ModelResolutionTest, with its
    // alias table.
    const TEST_ALIASES: &[(&str, &str)] = &[
        ("small", "haiku"),
        ("medium", "sonnet"),
        ("large", "opus"),
        ("extra-large", "fable"),
    ];

    #[test]
    fn size_alias_resolves_through_config_model_aliases() {
        assert_eq!(resolve_model_alias("medium", TEST_ALIASES), "sonnet");
    }

    #[test]
    fn alias_matching_is_case_insensitive_and_accepts_shorthand() {
        for (raw, want) in [
            ("Medium", "sonnet"),
            ("L", "opus"),
            ("XL", "fable"),
            ("SMALL", "haiku"),
            ("extra-large", "fable"),
        ] {
            assert_eq!(resolve_model_alias(raw, TEST_ALIASES), want, "{raw}");
        }
    }

    #[test]
    fn shorthand_set_matches_go_normalize_model_alias() {
        assert_eq!(
            MODEL_ALIAS_SHORTHAND,
            &[
                ("s", "small"),
                ("m", "medium"),
                ("l", "large"),
                ("xl", "extra-large")
            ]
        );
        assert_eq!(
            KNOWN_MODEL_ALIASES,
            &["small", "medium", "large", "extra-large"]
        );
        for raw in ["xlarge", "extra_large"] {
            assert_eq!(resolve_model_alias(raw, TEST_ALIASES), raw);
        }
    }

    #[test]
    fn unmapped_alias_falls_back_to_the_tier_name() {
        assert_eq!(resolve_model_alias("large", &[("small", "haiku")]), "large");
    }

    #[test]
    fn missing_aliases_fall_back_to_tier_name() {
        assert_eq!(resolve_model_alias("large", &[]), "large");
    }

    #[test]
    fn custom_non_tier_alias_keys_are_ignored() {
        assert_eq!(resolve_model_alias("fast", &[("fast", "haiku")]), "fast");
    }

    #[test]
    fn concrete_model_passes_through_unchanged() {
        assert_eq!(
            resolve_model_alias("claude-sonnet-4-5", TEST_ALIASES),
            "claude-sonnet-4-5"
        );
    }

    /// The tool names each preset denies, as `PolicyPreset::rules` lists
    /// them (`EDIT_TOOLS`, `SHELL_TOOLS`, `WEB_TOOLS`); `full` denies none.
    const READ_ONLY_DENIES: &[&str] = &[
        "Edit",
        "Write",
        "MultiEdit",
        "NotebookEdit",
        "fileChange",
        "write_file",
        "replace",
        "Bash",
        "BashOutput",
        "KillBash",
        "KillShell",
        "commandExecution",
        "run_shell_command",
        "shell",
        "exec_command",
        "WebFetch",
        "WebSearch",
        "web_fetch",
        "google_web_search",
    ];
    const EDIT_WORKTREE_DENIES: &[&str] = &[
        "Bash",
        "BashOutput",
        "KillBash",
        "KillShell",
        "commandExecution",
        "run_shell_command",
        "shell",
        "exec_command",
        "WebFetch",
        "WebSearch",
        "web_fetch",
        "google_web_search",
    ];

    /// The file a sandboxed turn's `read-only` policy gets, with telemetry
    /// on or off: the Claude Code names of its denials, bypass disabled.
    #[test]
    fn read_only_writes_its_claude_code_denials() {
        let deny = serde_json::json!([
            "Bash",
            "BashOutput",
            "Edit",
            "KillBash",
            "KillShell",
            "MultiEdit",
            "NotebookEdit",
            "WebFetch",
            "WebSearch",
            "Write"
        ]);
        let permissions =
            serde_json::json!({ "disableBypassPermissionsMode": "disable", "deny": deny });
        let off = managed_settings(READ_ONLY_DENIES, true);
        assert_eq!(
            parse(&off.json),
            serde_json::json!({
                "permissions": permissions,
                "env": telemetry_off_env(),
            })
        );
        assert_eq!(off.deny.len(), 10);
        let on = managed_settings(READ_ONLY_DENIES, false);
        assert_eq!(
            parse(&on.json),
            serde_json::json!({ "permissions": permissions })
        );
        assert!(!on.telemetry_off);
    }

    /// `edit-worktree` denies the shell and the web and nothing it edits.
    #[test]
    fn edit_worktree_writes_its_shell_and_web_denials() {
        let deny = serde_json::json!([
            "Bash",
            "BashOutput",
            "KillBash",
            "KillShell",
            "WebFetch",
            "WebSearch"
        ]);
        let permissions =
            serde_json::json!({ "disableBypassPermissionsMode": "disable", "deny": deny });
        assert_eq!(
            parse(&managed_settings(EDIT_WORKTREE_DENIES, true).json),
            serde_json::json!({ "permissions": permissions, "env": telemetry_off_env() })
        );
        assert_eq!(
            parse(&managed_settings(EDIT_WORKTREE_DENIES, false).json),
            serde_json::json!({ "permissions": permissions })
        );
    }

    /// `full` denies nothing, so there is no `deny` list; bypass mode is
    /// still disabled, since the drivers route every request through the
    /// branch's policy.
    #[test]
    fn full_writes_no_denials_only_the_bypass_setting() {
        let permissions = serde_json::json!({ "disableBypassPermissionsMode": "disable" });
        let off = managed_settings(&[], true);
        assert_eq!(
            parse(&off.json),
            serde_json::json!({ "permissions": permissions, "env": telemetry_off_env() })
        );
        assert!(off.deny.is_empty());
        assert_eq!(
            parse(&managed_settings(&[], false).json),
            serde_json::json!({ "permissions": permissions })
        );
    }

    /// Names of other harnesses are not written, and `mcp__` names are, as
    /// Claude Code spells MCP tools.
    #[test]
    fn only_claude_code_names_are_written() {
        let managed = managed_settings(&["shell", "Bash", "mcp__docs__search", "Bash"], false);
        assert_eq!(managed.deny, ["Bash", "mcp__docs__search"]);
    }

    /// Telemetry pinned off is what [`plan`] sets when telemetry is off, so
    /// the managed file never disagrees with the home's environment.
    #[test]
    fn pinned_telemetry_is_what_the_plan_sets_when_off() {
        let mut context = crate::Context::new(
            "claude-code",
            crate::Protocol::ClaudeStreamJson,
            "/home/branch",
            "/work",
        );
        context.telemetry = Some(crate::Telemetry {
            enabled: false,
            endpoint: None,
        });
        let plan = crate::plan(&context).unwrap();
        for (name, value) in TELEMETRY_OFF {
            let set = plan.env.iter().find(|var| var.name == *name);
            assert_eq!(set.map(|var| var.value.as_str()), Some(*value), "{name}");
        }
    }

    fn parse(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    fn telemetry_off_env() -> serde_json::Value {
        TELEMETRY_OFF
            .iter()
            .map(|(name, value)| ((*name).to_owned(), serde_json::json!(value)))
            .collect::<serde_json::Map<_, _>>()
            .into()
    }
}
