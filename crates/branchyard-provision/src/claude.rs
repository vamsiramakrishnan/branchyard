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
const CREDENTIALS: &str = ".claude/.credentials.json";
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
}
