// Derived from Scion (https://github.com/GoogleCloudPlatform/scion) at
// d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338:
// harnesses/opencode/provision.py. Copyright 2026 Google LLC. Licensed
// under the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to a sans-IO planner, for
// authentication only. The API key is set in the harness's environment
// (Scion relies on its host to project it); the auth file is written from
// the OPENCODE_AUTH secret. Not ported: Vertex AI, whose provider settings
// pin models in `.opencode.json`; the model setting, because Scion writes
// it to `~/.config/opencode/.opencode.json`, a file the `opencode acp`
// profile's OpenCode may not read, and a model that silently does not apply
// is worse than a refusal; the MCP writer, because the ACP driver passes
// MCP servers in the session; the models.dev catalog download, because
// planning does no I/O.

//! OpenCode: `opencode-acp`.

use crate::auth::{Method, Spec};
use crate::edit::Edit;
use crate::{
    needs_private_home, pass_session, unsupported, unused, Context, Plan, Provisioner, Refused, Via,
};

const AUTH_FILE: &str = ".local/share/opencode/auth.json";

pub const AUTH: Spec = Spec {
    harness: "opencode",
    methods: &[
        Method::env(
            "api-key",
            &["ANTHROPIC_API_KEY", "OPENAI_API_KEY"],
            "set ANTHROPIC_API_KEY or OPENAI_API_KEY",
        ),
        Method::file(
            "auth-file",
            "OPENCODE_AUTH",
            "provide OPENCODE_AUTH, the content of ~/.local/share/opencode/auth.json",
        ),
    ],
};

pub(crate) struct OpenCode;

impl Provisioner for OpenCode {
    fn harness(&self) -> &'static str {
        "opencode"
    }

    fn scion(&self) -> Option<&'static str> {
        Some("opencode")
    }

    fn plan(&self, context: &Context) -> Result<Plan, Refused> {
        let why = "Scion's OpenCode provisioner has no verified setting for it";
        if context.model.is_some() {
            return Err(unsupported(context, "a model", why));
        }
        if context.effort.is_some() {
            return Err(unsupported(context, "reasoning effort", why));
        }
        if context.telemetry.is_some() {
            return Err(unsupported(context, "telemetry", why));
        }
        let mut plan = Plan::default();
        pass_session(context, &mut plan);
        let given: Vec<&str> = context.secrets.iter().map(|s| s.name.as_str()).collect();
        if let Some(resolved) = AUTH.select(&given, context.auth.as_deref())? {
            plan.auth = Some(resolved.method.to_owned());
            match resolved.method {
                "api-key" => {
                    let key = resolved.env()?;
                    plan.secret_env(key, key, context.secret(key).unwrap_or_default(), true);
                }
                "auth-file" => {
                    needs_private_home(context, "OpenCode's auth.json")?;
                    let content = context.secret("OPENCODE_AUTH").unwrap_or_default();
                    if content.trim().is_empty() {
                        return Err(Refused("the OPENCODE_AUTH secret is empty".into()));
                    }
                    if serde_json::from_str::<serde_json::Value>(content).is_err() {
                        return Err(Refused("the OPENCODE_AUTH secret is not valid JSON".into()));
                    }
                    plan.edit(AUTH_FILE, true, Edit::Put(content.to_owned()));
                    plan.deliver(
                        "OPENCODE_AUTH",
                        Via::File {
                            path: AUTH_FILE.into(),
                        },
                        false,
                    );
                }
                _ => unreachable!("every method of AUTH is handled"),
            }
        }
        plan.unused_secrets = unused(context, &AUTH.names());
        Ok(plan)
    }
}
