// Derived from Scion (https://github.com/GoogleCloudPlatform/scion) at
// d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338: harnesses/codex/provision.py,
// the model aliases of harnesses/codex/config.yaml, and the tests of
// harnesses/codex/provision_test.py. Copyright 2026 Google LLC. Licensed
// under the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to a sans-IO planner.
// `model_reasoning_effort` is placed at the top level before the first
// table (Scion appends it after the last table, where TOML reads it as a
// key of that table); reasoning effort is taken as a level name, with
// Scion's 0-100 mapping in `Effort::from_level`; `[otel]` is reconciled
// only when telemetry is asked for, with the collector the task names,
// and the OpenTelemetry environment is always "production" (Scion reads it
// from its own variables); `CODEX_HOME` is set only in a private home;
// MCP servers and instructions stay on the driver's session channel
// (thread configuration and `developerInstructions`, or ACP), so Scion's
// `[mcp_servers.*]` writer and AGENTS.md projection are not used; the
// model is set in `config.toml` only for the ACP profile, whose driver has
// no model parameter (not from Scion). Scion's launch flags
// (`--dangerously-bypass-approvals-and-sandbox`, `--sandbox
// danger-full-access`) and its seed `approval_policy = "never"` are not
// ported: Branchyard's drivers route every approval.

//! Codex: `codex-app-server` and `codex-acp`.

use serde_json::json;

use crate::auth::{Method, Spec};
use crate::edit::{Edit, TomlEdit};
use crate::{
    needs_private_home, pass_session, toml, unused, Context, EnvVar, Plan, Protocol, Provisioner,
    Refused, Telemetry, Via,
};

const AUTH_FILE: &str = ".codex/auth.json";
const CONFIG: &str = ".codex/config.toml";

/// Scion's `model_aliases` for Codex.
pub const MODEL_ALIASES: &[(&str, &str)] = &[
    ("small", "gpt-5.6-luna"),
    ("medium", "gpt-5.6-terra"),
    ("large", "gpt-5.6-sol"),
    ("extra-large", "gpt-6-astra"),
];

pub const AUTH: Spec = Spec {
    harness: "codex",
    methods: &[
        Method::env(
            "api-key",
            &["CODEX_API_KEY", "OPENAI_API_KEY"],
            "set CODEX_API_KEY or OPENAI_API_KEY",
        ),
        Method::file(
            "auth-file",
            "CODEX_AUTH",
            "provide CODEX_AUTH, the content of ~/.codex/auth.json",
        ),
    ],
};

/// The `[otel]` table: export to the collector, never the user's prompts.
pub fn otel_section(telemetry: &Telemetry) -> String {
    if !telemetry.enabled {
        return "[otel]\nexporter = \"none\"\nmetrics_exporter = \"none\"\ntrace_exporter = \"none\"\n"
            .into();
    }
    let endpoint = toml::escape(telemetry.endpoint());
    let exporter = "otlp-grpc";
    [
        "[otel]".to_owned(),
        "environment = \"production\"".to_owned(),
        "log_user_prompt = false".to_owned(),
        format!("metrics_exporter.\"{exporter}\".endpoint = \"{endpoint}\""),
        format!("exporter.\"{exporter}\".endpoint = \"{endpoint}\""),
        format!("trace_exporter.\"{exporter}\".endpoint = \"{endpoint}\""),
    ]
    .join("\n")
        + "\n"
}

pub(crate) struct Codex;

impl Provisioner for Codex {
    fn harness(&self) -> &'static str {
        "codex"
    }

    fn scion(&self) -> Option<&'static str> {
        Some("codex")
    }

    fn plan(&self, context: &Context) -> Result<Plan, Refused> {
        let mut plan = Plan::default();
        pass_session(context, &mut plan);
        let given: Vec<&str> = context.secrets.iter().map(|s| s.name.as_str()).collect();
        if let Some(resolved) = AUTH.select(&given, context.auth.as_deref())? {
            needs_private_home(context, "Codex's auth.json")?;
            plan.auth = Some(resolved.method.to_owned());
            let content = match resolved.method {
                "api-key" => {
                    let key = resolved.env()?;
                    let value = context.secret(key).unwrap_or_default();
                    // Scion's json.dump(indent=2): its key order, not sorted.
                    let payload = json!({"auth_mode": "apikey", "OPENAI_API_KEY": value});
                    format!(
                        "{{\n  \"auth_mode\": {},\n  \"OPENAI_API_KEY\": {}\n}}\n",
                        payload["auth_mode"], payload["OPENAI_API_KEY"]
                    )
                }
                "auth-file" => {
                    let content = context.secret("CODEX_AUTH").unwrap_or_default();
                    if content.trim().is_empty() {
                        return Err(Refused("the CODEX_AUTH secret is empty".into()));
                    }
                    if serde_json::from_str::<serde_json::Value>(content).is_err() {
                        return Err(Refused("the CODEX_AUTH secret is not valid JSON".into()));
                    }
                    content.to_owned()
                }
                _ => unreachable!("every method of AUTH is handled"),
            };
            plan.edit(AUTH_FILE, true, Edit::Put(content));
            let secret = resolved.secret()?;
            plan.deliver(
                secret,
                Via::File {
                    path: AUTH_FILE.into(),
                },
                false,
            );
        }
        plan.unused_secrets = unused(context, &AUTH.names());

        let mut toml_edits = Vec::new();
        if let Some(effort) = context.effort {
            toml_edits.push(TomlEdit::RemoveKey("reasoning_effort".into()));
            toml_edits.push(TomlEdit::SetKey {
                key: "model_reasoning_effort".into(),
                literal: toml::string(effort.as_str()),
            });
        }
        if let Some(model) = &context.model {
            let model = crate::claude::resolve_model_alias(model, MODEL_ALIASES);
            match context.protocol {
                Protocol::CodexAppServer => plan.session.model = Some(model),
                _ => toml_edits.push(TomlEdit::SetKey {
                    key: "model".into(),
                    literal: toml::string(&model),
                }),
            }
        }
        if let Some(telemetry) = &context.telemetry {
            toml_edits.push(TomlEdit::RemoveTables("otel".into()));
            toml_edits.push(TomlEdit::AppendTable(otel_section(telemetry)));
        }
        if !toml_edits.is_empty() {
            needs_private_home(context, "Codex's config.toml")?;
            plan.edit(CONFIG, false, Edit::Toml(toml_edits));
        }
        if context.private_home && plan.touches_home_or_env() {
            plan.set_env(EnvVar::plain("CODEX_HOME", context.home_path(".codex")));
        }
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit::apply_all;
    use crate::Effort;

    fn reconcile(
        current: Option<&str>,
        effort: Option<Effort>,
        telemetry: Option<Telemetry>,
    ) -> String {
        let mut context = Context::new("codex", Protocol::CodexAppServer, "/h", "/w");
        context.private_home = true;
        context.effort = effort;
        context.telemetry = telemetry;
        let plan = Codex.plan(&context).unwrap();
        let file = plan.files.iter().find(|f| f.path == CONFIG).unwrap();
        apply_all(&file.edits, current).unwrap().unwrap()
    }

    fn on(endpoint: &str) -> Option<Telemetry> {
        Some(Telemetry {
            enabled: true,
            endpoint: Some(endpoint.into()),
        })
    }

    // From Scion's codex/provision_test.py.

    #[test]
    fn native_otel_routes_only_to_the_configured_receiver() {
        let section = otel_section(&on("http://127.0.0.1:14317").unwrap());
        for line in [
            "metrics_exporter.\"otlp-grpc\".endpoint = \"http://127.0.0.1:14317\"",
            "exporter.\"otlp-grpc\".endpoint = \"http://127.0.0.1:14317\"",
            "trace_exporter.\"otlp-grpc\".endpoint = \"http://127.0.0.1:14317\"",
            "log_user_prompt = false",
        ] {
            assert!(section.contains(line), "{line}");
        }
        assert!(!section.contains("statsig"));
    }

    #[test]
    fn disabled_otel_disables_all_exporters() {
        let out = reconcile(
            Some("[otel.exporter.\"otlp-grpc\"]\nendpoint = \"https://external.invalid:443\"\n"),
            None,
            Some(Telemetry {
                enabled: false,
                endpoint: None,
            }),
        );
        for line in [
            "exporter = \"none\"",
            "metrics_exporter = \"none\"",
            "trace_exporter = \"none\"",
        ] {
            assert!(out.contains(line), "{line}");
        }
        assert!(!out.contains("external.invalid"));
    }

    #[test]
    fn reasoning_effort_is_written_as_model_reasoning_effort() {
        let out = reconcile(None, Some(Effort::Medium), None);
        assert!(out.contains("model_reasoning_effort = \"medium\""));
    }

    #[test]
    fn no_effort_writes_no_reasoning_key() {
        let mut context = Context::new("codex", Protocol::CodexAppServer, "/h", "/w");
        context.private_home = true;
        let plan = Codex.plan(&context).unwrap();
        assert!(plan.files.is_empty() && plan.env.is_empty());
    }

    #[test]
    fn replaces_existing_reasoning_effort_keys() {
        for current in [
            "reasoning_effort = \"low\"\nother_key = \"value\"\n",
            "model_reasoning_effort = \"medium\"\nother_key = \"value\"\n",
            "model_reasoning_effort = \"medium\"\nreasoning_effort = \"low\"\nother_key = \"value\"\n",
        ] {
            let out = reconcile(Some(current), Some(Effort::High), None);
            assert!(out.contains("model_reasoning_effort = \"high\""), "{out}");
            assert_eq!(out.matches("model_reasoning_effort").count(), 1, "{out}");
            assert!(!out.contains("\"low\"") && !out.contains("\"medium\""), "{out}");
            assert!(out.contains("other_key = \"value\""), "{out}");
        }
    }

    #[test]
    fn reasoning_effort_levels_map_as_scion_maps_them() {
        for (level, effort) in [
            (0, Effort::Low),
            (25, Effort::Low),
            (26, Effort::Medium),
            (50, Effort::Medium),
            (51, Effort::High),
            (75, Effort::High),
            (76, Effort::Xhigh),
            (100, Effort::Xhigh),
            (-10, Effort::Low),
            (150, Effort::Xhigh),
        ] {
            assert_eq!(Effort::from_level(level), effort, "{level}");
        }
    }
}
