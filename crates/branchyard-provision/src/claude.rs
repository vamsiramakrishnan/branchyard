// Derived from Scion (https://github.com/GoogleCloudPlatform/scion) at
// d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338: harnesses/claude/provision.py,
// the model aliases of harnesses/claude/config.yaml, and the model tests of
// harnesses/claude/provision_test.py. Copyright 2026 Google LLC. Licensed
// under the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to a sans-IO planner.
// The API-key fingerprint is added to `customApiKeyResponses.approved`
// instead of replacing it, and the workspace is marked trusted without
// dropping other projects; a credentials file is written from the
// CLAUDE_AUTH secret rather than mounted; no model is set unless one is
// asked for (Scion defaults to "opus"); telemetry goes to the endpoint the
// task names, without Scion's cloud-provider checks, which belong to its
// own collector; MCP servers and instructions stay on the driver's session
// channel (`--mcp-config`, plugin or system prompt, or ACP), so Scion's
// `.claude.json` MCP merge and CLAUDE.md projection are not used; the
// version probe (`claude --version`) is dropped, since planning runs no
// processes. Scion's launch flags (`--dangerously-skip-permissions`) are
// not ported: Branchyard's drivers route every permission request.

//! Claude Code: `claude-code-stream-json` and `claude-code-acp`.

use serde_json::json;

use crate::auth::{Method, Spec};
use crate::edit::{path, Edit, JsonEdit};
use crate::{
    needs_private_home, pass_session, unsupported, unused, Context, EnvVar, Plan, Provisioner,
    Refused,
};

const CLAUDE_JSON: &str = ".claude.json";
const CREDENTIALS: &str = ".claude/.credentials.json";

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
                    let key = secret("ANTHROPIC_API_KEY");
                    plan.set_env(EnvVar::secret("ANTHROPIC_API_KEY", key.clone()));
                    needs_private_home(context, "the API key approval")?;
                    // Claude Code's approval fingerprint: the key's last 20
                    // characters. It is most of the key, so the file is
                    // treated as a secret.
                    let start = key.char_indices().rev().nth(19).map_or(0, |(i, _)| i);
                    plan.edit(
                        CLAUDE_JSON,
                        true,
                        Edit::Json {
                            edits: vec![
                                JsonEdit::Push(
                                    path(&["customApiKeyResponses", "approved"]),
                                    json!(&key[start..]),
                                ),
                                JsonEdit::Default(
                                    path(&["customApiKeyResponses", "rejected"]),
                                    json!([]),
                                ),
                            ],
                            comment_lines: false,
                        },
                    );
                }
                "oauth-token" => plan.set_env(EnvVar::secret(
                    "CLAUDE_CODE_OAUTH_TOKEN",
                    secret("CLAUDE_CODE_OAUTH_TOKEN"),
                )),
                "auth-file" => {
                    needs_private_home(context, "the credentials file")?;
                    let content = secret("CLAUDE_AUTH");
                    if serde_json::from_str::<serde_json::Value>(&content).is_err() {
                        return Err(Refused("the CLAUDE_AUTH secret is not valid JSON".into()));
                    }
                    plan.edit(CREDENTIALS, true, Edit::Put(content));
                }
                "vertex-ai" => {
                    let region = resolved.env_key.unwrap_or("GOOGLE_CLOUD_REGION");
                    plan.set_env(EnvVar::plain("CLAUDE_CODE_USE_VERTEX", "1"));
                    plan.set_env(EnvVar::secret(
                        "ANTHROPIC_VERTEX_PROJECT_ID",
                        secret("GOOGLE_CLOUD_PROJECT"),
                    ));
                    plan.set_env(EnvVar::secret("CLOUD_ML_REGION", secret(region)));
                }
                _ => unreachable!("every method of AUTH is handled"),
            }
            // Suppress model upgrade and fallback dialogs: the provider is
            // chosen here.
            plan.set_env(EnvVar::plain("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1"));
            if context.private_home {
                trust_workspace(context, &mut plan);
            }
        }
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
            plan.set_env(EnvVar::plain("ANTHROPIC_MODEL", model));
        }
        Ok(plan)
    }
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
