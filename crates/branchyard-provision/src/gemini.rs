// Derived from Scion (https://github.com/GoogleCloudPlatform/scion) at
// d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338:
// harnesses/gemini-cli/provision.py. Copyright 2026 Google LLC. Licensed
// under the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to a sans-IO planner.
// The OAuth credentials file is written from the GEMINI_OAUTH_CREDS secret
// rather than mounted; Vertex AI takes the project and location from the
// secrets given, not the container's environment; settings.json is merged
// key by key and refused, not reset, when it cannot be parsed; telemetry
// goes to the collector the task names and is reconciled only when asked
// for; the native system prompt (GEMINI_SYSTEM_MD) and GEMINI.md
// projection are not used, because the ACP driver passes instructions in
// the session. Scion's `--yolo` launch flag and `"yolo": true` seed setting
// are not ported: Branchyard's ACP driver routes every permission request.

//! Gemini CLI: `gemini-cli-acp`.

use serde_json::json;

use crate::auth::{Method, Spec};
use crate::edit::{path, Edit, JsonEdit};
use crate::{
    needs_private_home, pass_session, unsupported, unused, Context, EnvVar, Plan, Provisioner,
    Refused,
};

const SETTINGS: &str = ".gemini/settings.json";
const OAUTH_CREDS: &str = ".gemini/oauth_creds.json";

pub const AUTH: Spec = Spec {
    harness: "gemini-cli",
    methods: &[
        Method::env(
            "api-key",
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            "set GEMINI_API_KEY or GOOGLE_API_KEY",
        ),
        Method::file(
            "auth-file",
            "GEMINI_OAUTH_CREDS",
            "provide GEMINI_OAUTH_CREDS, the content of ~/.gemini/oauth_creds.json",
        ),
        Method::env(
            "vertex-ai",
            &["GOOGLE_CLOUD_PROJECT"],
            "set GOOGLE_CLOUD_PROJECT (with ADC available to the harness) for Vertex AI",
        ),
    ],
};

/// Scion's map from a method to Gemini CLI's `security.auth.selectedType`.
pub fn selected_type(method: &str) -> Option<&'static str> {
    match method {
        "api-key" => Some("gemini-api-key"),
        "auth-file" => Some("oauth-personal"),
        "vertex-ai" => Some("vertex-ai"),
        _ => None,
    }
}

pub(crate) struct Gemini;

impl Provisioner for Gemini {
    fn harness(&self) -> &'static str {
        "gemini-cli"
    }

    fn scion(&self) -> Option<&'static str> {
        Some("gemini-cli")
    }

    fn plan(&self, context: &Context) -> Result<Plan, Refused> {
        if context.effort.is_some() {
            return Err(unsupported(
                context,
                "reasoning effort",
                "Scion's Gemini CLI provisioner has no setting for it",
            ));
        }
        let mut plan = Plan::default();
        pass_session(context, &mut plan);
        let mut settings = Vec::new();
        let given: Vec<&str> = context.secrets.iter().map(|s| s.name.as_str()).collect();
        if let Some(resolved) = AUTH.select(&given, context.auth.as_deref())? {
            plan.auth = Some(resolved.method.to_owned());
            let secret = |name: &str| context.secret(name).unwrap_or_default().to_owned();
            match resolved.method {
                "api-key" => {
                    let key = resolved.env_key.expect("an env method has a key");
                    plan.set_env(EnvVar::secret(key, secret(key)));
                }
                "auth-file" => {
                    let content = secret("GEMINI_OAUTH_CREDS");
                    if serde_json::from_str::<serde_json::Value>(&content).is_err() {
                        return Err(Refused(
                            "the GEMINI_OAUTH_CREDS secret is not valid JSON".into(),
                        ));
                    }
                    plan.edit(OAUTH_CREDS, true, Edit::Put(content));
                }
                "vertex-ai" => {
                    plan.set_env(EnvVar::secret(
                        "GOOGLE_CLOUD_PROJECT",
                        secret("GOOGLE_CLOUD_PROJECT"),
                    ));
                    for name in ["GOOGLE_CLOUD_REGION", "GOOGLE_CLOUD_LOCATION"] {
                        if let Some(value) = context.secret(name) {
                            plan.set_env(EnvVar::secret(name, value));
                        }
                    }
                }
                _ => unreachable!("every method of AUTH is handled"),
            }
            let selected = selected_type(resolved.method).expect("every method maps");
            settings.push(JsonEdit::Set(
                path(&["security", "auth", "selectedType"]),
                json!(selected),
            ));
        }
        let mut names = AUTH.names();
        names.extend(["GOOGLE_CLOUD_REGION", "GOOGLE_CLOUD_LOCATION"]);
        plan.unused_secrets = unused(context, &names);
        if let Some(telemetry) = &context.telemetry {
            let on = telemetry.enabled;
            let endpoint = telemetry.endpoint();
            for (name, value) in [
                (
                    "GEMINI_TELEMETRY_ENABLED",
                    if on { "true" } else { "false" },
                ),
                ("GEMINI_TELEMETRY_TARGET", "local"),
                ("GEMINI_TELEMETRY_OTLP_ENDPOINT", endpoint),
                ("GEMINI_TELEMETRY_OTLP_PROTOCOL", "grpc"),
                ("GEMINI_TELEMETRY_LOG_PROMPTS", "false"),
                ("GEMINI_TELEMETRY_TRACES_ENABLED", "false"),
                ("GEMINI_TELEMETRY_USE_COLLECTOR", "false"),
                ("GEMINI_TELEMETRY_OUTFILE", ""),
            ] {
                plan.set_env(EnvVar::plain(name, value));
            }
            settings.push(JsonEdit::Set(
                path(&["telemetry"]),
                json!({
                    "enabled": on, "target": "local", "otlpEndpoint": endpoint,
                    "otlpProtocol": "grpc", "logPrompts": false, "traces": false,
                    "useCollector": false,
                }),
            ));
        }
        if let Some(model) = &context.model {
            settings.push(JsonEdit::Set(path(&["model", "name"]), json!(model)));
        }
        if !settings.is_empty() {
            needs_private_home(context, "Gemini CLI's settings.json")?;
            plan.edit(
                SETTINGS,
                false,
                Edit::Json {
                    edits: settings,
                    comment_lines: false,
                },
            );
        }
        Ok(plan)
    }
}
