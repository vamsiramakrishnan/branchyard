// Derived from Scion (https://github.com/GoogleCloudPlatform/scion) at
// d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338:
// harnesses/antigravity/provision.py (its api-key method, AGY_MCP_MAPPING
// with apply_mcp_servers_simple and _translate_simple of
// harnesses/scion_harness.py, and _copy_instructions) and the
// instructions_file of harnesses/antigravity/config.yaml. Copyright 2026
// Google LLC. Licensed under the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to a sans-IO planner.
// MCP servers are merged by name into `mcp_config.json`, and the ones
// Branchyard installed earlier but no longer asks for are removed, leaving
// the others; instructions go between markers in GEMINI.md instead of
// replacing the file. Not ported: the Vertex AI (ADC) and AGY_TOKEN
// methods, which need a version probe (`agy --version`) and a generated
// launch wrapper; the thinking tier, which Scion passes through that
// wrapper; the onboarding files and hooks, which serve Scion's interactive
// terminal launch rather than the headless stream Branchyard drives.

//! Antigravity CLI: `antigravity-stream-json`. Its driver has no channel
//! for MCP servers or instructions, so they go into its configuration, in
//! a private home.

use serde_json::{json, Map, Value};

use crate::auth::{Method, Spec};
use crate::edit::{path, Edit, JsonEdit};
use crate::{
    needs_private_home, unsupported, unused, Context, McpServer, Plan, Provisioner, Refused,
};

const MCP_CONFIG: &str = ".gemini/config/mcp_config.json";
const INSTRUCTIONS: &str = ".gemini/GEMINI.md";

pub const AUTH: Spec = Spec {
    harness: "antigravity",
    methods: &[Method::env(
        "api-key",
        &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        "set GEMINI_API_KEY or GOOGLE_API_KEY",
    )],
};

/// A server as Antigravity's `mcpServers` entry: Scion's simple mapping,
/// with the transport as `type`.
pub fn mcp_entry(server: &McpServer) -> Value {
    let mut entry = Map::new();
    entry.insert("type".into(), json!("stdio"));
    entry.insert("command".into(), json!(server.command));
    if !server.args.is_empty() {
        entry.insert("args".into(), json!(server.args));
    }
    if !server.env.is_empty() {
        let env: Map<String, Value> = server
            .env
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        entry.insert("env".into(), Value::Object(env));
    }
    Value::Object(entry)
}

pub(crate) struct Antigravity;

impl Provisioner for Antigravity {
    fn harness(&self) -> &'static str {
        "antigravity"
    }

    fn scion(&self) -> Option<&'static str> {
        Some("antigravity")
    }

    #[allow(clippy::expect_used)] // ratchet: branchyard-provision
    fn plan(&self, context: &Context) -> Result<Plan, Refused> {
        let why = "Branchyard's translation of Scion's Antigravity provisioner has no setting \
                   for it";
        if context.effort.is_some() {
            return Err(unsupported(context, "reasoning effort", why));
        }
        if context.telemetry.is_some() {
            return Err(unsupported(context, "telemetry", why));
        }
        let mut plan = Plan::default();
        plan.session.model = context.model.clone();
        let given: Vec<&str> = context.secrets.iter().map(|s| s.name.as_str()).collect();
        if let Some(resolved) = AUTH.select(&given, context.auth.as_deref())? {
            plan.auth = Some(resolved.method.to_owned());
            let key = resolved.env_key.expect("an env method has a key");
            plan.secret_env(key, key, context.secret(key).unwrap_or_default(), true);
        }
        plan.unused_secrets = unused(context, &AUTH.names());

        // MCP servers: merged by name; stale ones Branchyard installed go.
        if !context.mcp_servers.is_empty() {
            needs_private_home(context, "MCP servers")?;
        }
        let names: Vec<String> = context.mcp_servers.iter().map(|s| s.name.clone()).collect();
        if context.private_home && (!names.is_empty() || !context.installed.mcp_servers.is_empty())
        {
            let mut edits: Vec<JsonEdit> = context
                .installed
                .mcp_servers
                .iter()
                .filter(|name| !names.contains(name))
                .map(|name| JsonEdit::Remove(path(&["mcpServers", name])))
                .collect();
            edits.extend(context.mcp_servers.iter().map(|server| {
                JsonEdit::Set(path(&["mcpServers", &server.name]), mcp_entry(server))
            }));
            // A server's variables may carry a token, such as Branchyard's
            // delegation token.
            let secret = context.mcp_servers.iter().any(|s| !s.env.is_empty());
            plan.edit(
                MCP_CONFIG,
                secret,
                Edit::Json {
                    edits,
                    comment_lines: false,
                },
            );
            plan.installed_mcp_servers = Some(names);
        }

        // Instructions: Branchyard's block in GEMINI.md.
        match (&context.instructions, context.private_home) {
            (Some(_), false) => needs_private_home(context, "Standing instructions")?,
            (instructions, true) => plan.edit(
                INSTRUCTIONS,
                false,
                Edit::Block(
                    instructions
                        .as_ref()
                        .map(|i| crate::instructions::section("Agent Instructions", &i.text)),
                ),
            ),
            (None, false) => {}
        }
        Ok(plan)
    }
}
