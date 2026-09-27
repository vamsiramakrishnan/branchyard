// Derived from Scion (https://github.com/GoogleCloudPlatform/scion) at
// d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338: harnesses/copilot/provision.py.
// Copyright 2026 Google LLC. Licensed under the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to a sans-IO planner.
// The token comes only from the secrets given (no fallback to the
// container's environment); the COPILOT_CONFIG file is validated, then the
// workspace is added to its `trustedFolders`; settings defaults and trusted
// folders are written only alongside provisioned authentication, in a
// private home; telemetry goes to the collector the task names, without
// Scion's header and CA overrides, and "off" writes nothing, as in Scion;
// not ported: the MCP writer and instructions projection (the ACP driver
// passes both in the session) and the sciontool hooks file, which only
// Scion's own hook receiver reads.

//! GitHub Copilot CLI: `github-copilot-acp`.

use serde_json::json;

use crate::auth::{Method, Spec};
use crate::edit::{apply_all, path, Edit, JsonEdit};
use crate::{
    needs_private_home, pass_session, unsupported, unused, Context, EnvVar, Plan, Provisioner,
    Refused,
};

const CONFIG: &str = ".copilot/config.json";
const SETTINGS: &str = ".copilot/settings.json";

pub const AUTH: Spec = Spec {
    harness: "github-copilot",
    methods: &[
        Method::env(
            "api-key",
            &["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"],
            "set COPILOT_GITHUB_TOKEN, GH_TOKEN, or GITHUB_TOKEN with a fine-grained PAT that \
             has \"Copilot Requests\" permission",
        ),
        Method::file(
            "auth-file",
            "COPILOT_CONFIG",
            "provide COPILOT_CONFIG, the content of ~/.copilot/config.json",
        ),
    ],
};

pub(crate) struct Copilot;

impl Provisioner for Copilot {
    fn harness(&self) -> &'static str {
        "github-copilot"
    }

    fn scion(&self) -> Option<&'static str> {
        Some("copilot")
    }

    fn plan(&self, context: &Context) -> Result<Plan, Refused> {
        let why = "Scion's Copilot provisioner has no setting for it";
        if context.model.is_some() {
            return Err(unsupported(context, "a model", why));
        }
        if context.effort.is_some() {
            return Err(unsupported(context, "reasoning effort", why));
        }
        let mut plan = Plan::default();
        pass_session(context, &mut plan);
        let given: Vec<&str> = context.secrets.iter().map(|s| s.name.as_str()).collect();
        let resolved = AUTH.select(&given, context.auth.as_deref())?;
        let trust = Edit::Json {
            edits: vec![JsonEdit::Push(
                path(&["trustedFolders"]),
                json!(context.workspace),
            )],
            comment_lines: true,
        };
        if let Some(resolved) = resolved {
            plan.auth = Some(resolved.method.to_owned());
            needs_private_home(context, "Copilot's settings")?;
            match resolved.method {
                "api-key" => {
                    let key = resolved.env_key.expect("an env method has a key");
                    let token = context.secret(key).unwrap_or_default();
                    plan.set_env(EnvVar::secret("COPILOT_GITHUB_TOKEN", token));
                    plan.edit(CONFIG, false, trust);
                }
                "auth-file" => {
                    let content = context.secret("COPILOT_CONFIG").unwrap_or_default();
                    if content.trim().is_empty() {
                        return Err(Refused("the COPILOT_CONFIG secret is empty".into()));
                    }
                    let trusted = apply_all(&[trust], Some(content)).map_err(|_| {
                        Refused("the COPILOT_CONFIG secret is not valid JSON".into())
                    })?;
                    plan.edit(CONFIG, true, Edit::Put(trusted.unwrap_or_default()));
                }
                _ => unreachable!("every method of AUTH is handled"),
            }
            plan.edit(
                SETTINGS,
                false,
                Edit::Json {
                    edits: vec![
                        JsonEdit::Default(path(&["autoUpdate"]), json!(false)),
                        JsonEdit::Default(path(&["banner"]), json!("never")),
                    ],
                    comment_lines: false,
                },
            );
        }
        plan.unused_secrets = unused(context, &AUTH.names());
        if let Some(telemetry) = context.telemetry.as_ref().filter(|t| t.enabled) {
            for (name, value) in [
                ("COPILOT_TELEMETRY_ENABLED", "true"),
                ("OTEL_EXPORTER_OTLP_ENDPOINT", telemetry.endpoint()),
                ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
                ("OTEL_METRICS_EXPORTER", "otlp"),
                ("OTEL_LOGS_EXPORTER", "otlp"),
                ("OTEL_METRIC_EXPORT_INTERVAL", "30000"),
            ] {
                plan.set_env(EnvVar::plain(name, value));
            }
        }
        Ok(plan)
    }
}
