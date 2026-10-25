// Derived from Scion (https://github.com/GoogleCloudPlatform/scion) at
// d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338:
// harnesses/opencode/provision.py. Copyright 2026 Google LLC. Licensed
// under the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to a sans-IO planner, for
// authentication, the model and the tools OpenCode asks about. The API key
// is set in the harness's environment (Scion relies on its host to project
// it); the auth file is written from the OPENCODE_AUTH secret. Not ported:
// Vertex AI, whose provider settings pin models in `.opencode.json`; the
// model setting's file, because Scion writes it to
// `~/.config/opencode/.opencode.json`, which OpenCode 1.19.4 does not read
// (`opencode.json` in the same directory is the file it reads: `model` in
// it chose the model of `opencode acp`, checked 25 October 2026); the MCP
// writer, because the ACP driver passes MCP servers in the session; the
// models.dev catalog download, because planning does no I/O.

//! OpenCode: `opencode-acp`. `opencode acp` takes MCP servers and
//! instructions on the session channel and asks its client before a tool
//! only when the tool's `permission` is `ask`; its default agent allows
//! everything (`opencode debug agent build`: `"*": allow`), so out of the
//! box no request ever reaches the driver, Branchyard's policy decides
//! nothing, and an interrupt waits for a request that does not come
//! (qualification, 25 October 2026: 6 of 9 until the file below asked). In
//! a private home, `~/.config/opencode/opencode.json` is therefore written
//! with `bash`, `edit` and `webfetch` set to `ask`, and the model (ACP v1
//! has no model parameter) as `model`. A home that is yours keeps your
//! `permission` settings, so there the policy reaches only what you ask
//! OpenCode to ask about.

use serde_json::json;

use crate::auth::{Method, Spec};
use crate::edit::{path, Edit, JsonEdit};
use crate::{
    needs_private_home, pass_session, unsupported, unused, Context, Plan, Provisioner, Refused, Via,
};

const AUTH_FILE: &str = ".local/share/opencode/auth.json";

/// OpenCode's configuration, in its global configuration directory.
pub const CONFIG: &str = ".config/opencode/opencode.json";

/// The tools OpenCode asks its client about when their `permission` is
/// `ask`: shell commands, file edits (writes and patches too) and web
/// fetches.
pub const ASKED: &[&str] = &["bash", "edit", "webfetch"];

pub const AUTH: Spec = Spec {
    harness: "opencode",
    methods: &[
        Method::env(
            "api-key",
            // OpenCode 1.19.4's Google provider reads
            // GOOGLE_GENERATIVE_AI_API_KEY; GEMINI_API_KEY, which
            // `opencode providers list` offers for it, is not passed on
            // (checked 25 October 2026), so it is not a method.
            &[
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
                "GOOGLE_GENERATIVE_AI_API_KEY",
            ],
            "set ANTHROPIC_API_KEY, OPENAI_API_KEY or GOOGLE_GENERATIVE_AI_API_KEY",
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
            needs_private_home(context, "The model (model in OpenCode's opencode.json)")?;
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
        // opencode.json: the model, set when asked for and taken back when
        // not, and an `ask` for each tool OpenCode would otherwise run
        // without asking. Only in a private home: a home that is yours
        // keeps its own settings.
        if context.private_home {
            let mut edits = vec![match &context.model {
                Some(model) => JsonEdit::Set(path(&["model"]), json!(model)),
                None => JsonEdit::Remove(path(&["model"])),
            }];
            for tool in ASKED {
                edits.push(JsonEdit::Set(path(&["permission", tool]), json!("ask")));
            }
            plan.edit(
                CONFIG,
                false,
                Edit::Json {
                    edits,
                    comment_lines: false,
                },
            );
        }
        Ok(plan)
    }
}
