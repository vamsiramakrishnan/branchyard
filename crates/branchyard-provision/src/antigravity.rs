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
// the others; an HTTP server is written as `serverUrl` and `headers`;
// instructions go between markers in GEMINI.md instead of replacing the
// file; the branch's permission policy becomes `permissions` rules in the
// CLI's settings, which Scion leaves to `--dangerously-skip-permissions`.
// Not ported: the Vertex AI (ADC) and AGY_TOKEN methods, which need a
// version probe (`agy --version`) and a generated launch wrapper; the
// thinking tier, which Scion passes through that wrapper; the onboarding
// files and hooks, which serve Scion's interactive terminal launch rather
// than the headless stream Branchyard drives.

//! Antigravity CLI: `antigravity-stream-json`. Its driver has no channel
//! for MCP servers or instructions, so they go into its configuration, in
//! a private home.
//!
//! Its headless stream carries no permission requests either: a tool that
//! needs approval is soft-denied unless a rule under `permissions.allow` in
//! `~/.gemini/antigravity-cli/settings.json` allows it
//! (<https://antigravity.google/docs/cli/headless/>). The branch's policy
//! ([`Context::permissions`]) becomes those rules here; see [`rules`].

use serde_json::{json, Map, Value};

use crate::auth::{Method, Spec};
use crate::edit::{path, Edit, JsonEdit};
use crate::permissions::{Permissions, PermissionsWritten};
use crate::{
    needs_private_home, unsupported, unused, Context, McpServer, Plan, Provisioner, Refused,
    RemoteMcpServer, RemoteTransport,
};

const MCP_CONFIG: &str = ".gemini/config/mcp_config.json";
const INSTRUCTIONS: &str = ".gemini/GEMINI.md";
/// The CLI's settings, which hold its permission rules.
pub const SETTINGS: &str = ".gemini/antigravity-cli/settings.json";

/// The CLI's shell tool, by the name its stream gives it
/// (`step_update.tool_name`, seen from agy 1.3.3).
pub const SHELL_TOOL: &str = "run_command";

/// The tool name a policy denies to stop file edits: the CLI's own
/// permission action, and the name in Branchyard's list of edit tools.
const WRITE_TOOL: &str = "write_file";

/// The actions of the CLI's permission rules, `action(target)`, from its
/// permissions page (<https://antigravity.google/docs/permissions?tab=cli>).
const ACTIONS: &[&str] = &[
    "read_file",
    "write_file",
    "read_url",
    "execute_url",
    "command",
    "unsandboxed",
    "mcp",
];

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

/// A streamable HTTP server as Antigravity's `mcpServers` entry: `serverUrl`
/// and `headers`, the keys its MCP page documents
/// (<https://antigravity.google/docs/mcp>); agy 1.3.3's own MCP guide also
/// accepts `serverUrl`.
pub fn remote_entry(server: &RemoteMcpServer) -> Value {
    let mut entry = Map::new();
    entry.insert("serverUrl".into(), json!(server.url));
    if !server.headers.is_empty() {
        let headers: Map<String, Value> = server
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        entry.insert("headers".into(), Value::Object(headers));
    }
    Value::Object(entry)
}

/// The rules the CLI reads, from the branch's policy. Never wider than the
/// policy: what it would ask a person about is left to the CLI's default,
/// which soft-denies it headless. In order:
///
/// - The policy's own rules already in the CLI's form (`command(cargo
///   test)`, `mcp(docs/search)`, ...), allowed or denied as the policy says.
/// - Commands, when the policy allows the shell ([`SHELL_TOOL`]): `git`, and
///   `by` when the turn's harness has it. When it allows only Branchyard's
///   delegation commands: `by` alone. Each as a regular expression over the
///   line from the program on (`command(regex:git .*)`), never a bare
///   program prefix, which agy 1.3.3 did not match against a line with a
///   redirection; a `by` path holding whitespace gets no rule of its own.
/// - Each MCP server the turn gives the harness: `mcp(<server>/*)` when the
///   policy allows all its tools (`mcp__<server>__*`, `mcp__*`, the
///   fallback; for Branchyard's own server, also its delegation-command
///   rule), and `mcp(<server>/<tool>)` for a rule naming one tool.
/// - `write_file(*)` denied when the policy denies file edits: the CLI
///   allows writes in the workspace by default.
pub fn rules(context: &Context, permissions: &Permissions) -> PermissionsWritten {
    let mut allow: Vec<String> = Vec::new();
    let mut deny: Vec<String> = Vec::new();
    for rule in permissions
        .rules
        .iter()
        .filter(|r| r.delegation_by.is_none() && native(&r.tool))
    {
        match rule.allow {
            true => allow.push(rule.tool.clone()),
            false => deny.push(rule.tool.clone()),
        }
    }
    let shell = permissions.decide(SHELL_TOOL) == Some(true);
    let delegation = permissions.delegation_by(SHELL_TOOL);
    if shell {
        allow.extend(command("git"));
    }
    if let Some(by) = permissions
        .by
        .as_deref()
        .filter(|_| shell || delegation.is_some())
    {
        allow.extend(command("by"));
        allow.extend(command(by));
        allow.extend(delegation.and_then(command));
    }
    let servers = context
        .mcp_servers
        .iter()
        .map(|s| s.name.as_str())
        .chain(context.remote_mcp_servers.iter().map(|s| s.name.as_str()));
    for server in servers {
        for (tool, allowed) in permissions.mcp_tool_rules(server) {
            let rule = format!("mcp({server}/{tool})");
            match allowed {
                true => allow.push(rule),
                false => deny.push(rule),
            }
        }
        if permissions.decide_server(server) == Some(true) {
            allow.push(format!("mcp({server}/*)"));
        }
    }
    if permissions.decide(WRITE_TOOL) == Some(false) {
        deny.push("write_file(*)".into());
    }
    PermissionsWritten {
        path: SETTINGS.into(),
        allow: distinct(allow),
        deny: distinct(deny),
    }
}

/// Whether `tool` is already a rule in the CLI's form, `action(target)`.
fn native(tool: &str) -> bool {
    tool.split_once('(').is_some_and(|(action, rest)| {
        ACTIONS.contains(&action) && rest.len() > 1 && rest.ends_with(')')
    })
}

/// `command(regex:<program> .*)`, with the program's regular-expression
/// characters escaped; `None` for a program holding whitespace, which the
/// CLI splits a rule's pattern on.
fn command(program: &str) -> Option<String> {
    if program.is_empty() || program.contains(char::is_whitespace) {
        return None;
    }
    let mut escaped = String::new();
    for c in program.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    Some(format!("command(regex:{escaped} .*)"))
}

/// `rules` without repeats, in their first order.
fn distinct(rules: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for rule in rules {
        if !out.contains(&rule) {
            out.push(rule);
        }
    }
    out
}

pub(crate) struct Antigravity;

impl Provisioner for Antigravity {
    fn harness(&self) -> &'static str {
        "antigravity"
    }

    fn scion(&self) -> Option<&'static str> {
        Some("antigravity")
    }

    fn plan(&self, context: &Context) -> Result<Plan, Refused> {
        let why = "Branchyard's translation of Scion's Antigravity provisioner has no setting \
                   for it";
        if context.effort.is_some() {
            return Err(unsupported(context, "reasoning effort", why));
        }
        if context.telemetry.is_some() {
            return Err(unsupported(context, "telemetry", why));
        }
        if let Some(server) = context
            .remote_mcp_servers
            .iter()
            .find(|s| s.transport == RemoteTransport::Sse)
        {
            return Err(unsupported(
                context,
                &format!("the SSE MCP server {}", server.name),
                "agy 1.3.3's MCP guide says the legacy HTTP+SSE transport is not supported; \
                 give it a streamable HTTP endpoint",
            ));
        }
        let mut plan = Plan::default();
        plan.session.model = context.model.clone();
        let given: Vec<&str> = context.secrets.iter().map(|s| s.name.as_str()).collect();
        if let Some(resolved) = AUTH.select(&given, context.auth.as_deref())? {
            plan.auth = Some(resolved.method.to_owned());
            let key = resolved.env()?;
            // agy 1.3.3 reads the key from GEMINI_API_KEY alone (a
            // GOOGLE_API_KEY has no effect), and only with `modelProvider`
            // set to `gemini` in its settings; otherwise it asks for a
            // Google sign-in, which a headless run cannot give. The setting
            // goes into the private home every secret needs.
            plan.secret_env(
                key,
                "GEMINI_API_KEY",
                context.secret(key).unwrap_or_default(),
                true,
            );
            plan.edit(
                SETTINGS,
                false,
                Edit::Json {
                    edits: vec![JsonEdit::Set(path(&["modelProvider"]), json!("gemini"))],
                    comment_lines: false,
                },
            );
        }
        plan.unused_secrets = unused(context, &AUTH.names());

        // MCP servers: merged by name; stale ones Branchyard installed go.
        if !context.mcp_servers.is_empty() || !context.remote_mcp_servers.is_empty() {
            needs_private_home(context, "MCP servers")?;
        }
        let names: Vec<String> = context
            .mcp_servers
            .iter()
            .map(|s| s.name.clone())
            .chain(context.remote_mcp_servers.iter().map(|s| s.name.clone()))
            .collect();
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
            edits.extend(context.remote_mcp_servers.iter().map(|server| {
                JsonEdit::Set(path(&["mcpServers", &server.name]), remote_entry(server))
            }));
            // A server's variables or headers may carry a token, such as
            // Branchyard's delegation token.
            let secret = context.mcp_servers.iter().any(|s| !s.env.is_empty())
                || context
                    .remote_mcp_servers
                    .iter()
                    .any(|s| !s.headers.is_empty());
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

        // Permission rules: the branch's policy, in a private home only. A
        // home that is yours keeps the rules you set.
        if context.private_home
            && (context.permissions.is_some() || context.installed.permissions.is_some())
        {
            let written = match &context.permissions {
                Some(permissions) => rules(context, permissions),
                None => PermissionsWritten {
                    path: SETTINGS.into(),
                    ..PermissionsWritten::default()
                },
            };
            let before = context.installed.permissions.clone().unwrap_or_default();
            let mut edits = Vec::new();
            for (list, now, earlier) in [
                ("allow", &written.allow, &before.allow),
                ("deny", &written.deny, &before.deny),
            ] {
                let key = path(&["permissions", list]);
                edits.extend(
                    earlier
                        .iter()
                        .filter(|rule| !now.contains(rule))
                        .map(|rule| JsonEdit::Pull(key.clone(), json!(rule))),
                );
                edits.extend(
                    now.iter()
                        .map(|rule| JsonEdit::Push(key.clone(), json!(rule))),
                );
            }
            if !edits.is_empty() {
                plan.edit(
                    SETTINGS,
                    false,
                    Edit::Json {
                        edits,
                        comment_lines: false,
                    },
                );
            }
            plan.permissions = Some(written);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{Fallback, PermissionRule, DELEGATION_SERVER};
    use crate::Protocol;

    fn rule(tool: &str, allow: bool) -> PermissionRule {
        PermissionRule {
            tool: tool.into(),
            allow,
            delegation_by: None,
        }
    }

    fn context(servers: &[&str]) -> Context {
        let mut context = Context::new(
            "antigravity",
            Protocol::AntigravityStreamJson,
            "/home/branch",
            "/work",
        );
        context.mcp_servers = servers
            .iter()
            .map(|name| McpServer {
                name: (*name).into(),
                command: "/bin/true".into(),
                args: Vec::new(),
                env: Vec::new(),
            })
            .collect();
        context
    }

    /// `full`, with Branchyard's server and `by` on the harness's path:
    /// git and `by` as regular expressions, never `command(regex:.*)`, and
    /// every tool of each server.
    #[test]
    fn an_allowed_shell_gets_git_and_by_never_every_command() {
        let permissions = Permissions {
            by: Some("/opt/by.d/bin/by".into()),
            ..Permissions::new(Fallback::Allow)
        };
        let written = rules(&context(&[DELEGATION_SERVER, "docs"]), &permissions);
        assert_eq!(
            written.allow,
            [
                "command(regex:git .*)",
                "command(regex:by .*)",
                "command(regex:/opt/by\\.d/bin/by .*)",
                "mcp(branchyard/*)",
                "mcp(docs/*)",
            ]
        );
        assert!(written.deny.is_empty());
        assert!(!written.allow.iter().any(|r| r.contains("regex:.*")));
    }

    /// The delegation rule alone allows `by` and Branchyard's tools, not
    /// git, not another server's tools; a denying fallback denies writes.
    #[test]
    fn delegation_commands_alone_allow_by_and_branchyards_tools() {
        let permissions = Permissions {
            rules: vec![PermissionRule {
                tool: "*".into(),
                allow: true,
                delegation_by: Some("/usr/local/bin/by".into()),
            }],
            by: Some("/usr/local/bin/by".into()),
            ..Permissions::new(Fallback::Deny)
        };
        let written = rules(&context(&[DELEGATION_SERVER, "docs"]), &permissions);
        assert_eq!(
            written.allow,
            [
                "command(regex:by .*)",
                "command(regex:/usr/local/bin/by .*)",
                "mcp(branchyard/*)",
            ]
        );
        assert_eq!(written.deny, ["write_file(*)"]);
    }

    /// A policy that asks allows nothing it would have asked about, and
    /// passes through the rules it states in the CLI's form; a rule naming
    /// one MCP tool becomes one for that tool.
    #[test]
    fn what_a_policy_asks_about_is_not_allowed_and_native_rules_pass_through() {
        let permissions = Permissions {
            rules: vec![
                rule("command(regex:cargo (build|test) .*)", true),
                rule("command(rm)", false),
                rule("mcp__docs__search", true),
                rule("mcp__docs__drop", false),
                rule("Bash", true),
                rule("command(", true),
            ],
            by: Some("/usr/local/bin/by".into()),
            ..Permissions::new(Fallback::Ask)
        };
        let written = rules(&context(&[DELEGATION_SERVER, "docs"]), &permissions);
        assert_eq!(
            written.allow,
            ["command(regex:cargo (build|test) .*)", "mcp(docs/search)"]
        );
        assert_eq!(written.deny, ["command(rm)", "mcp(docs/drop)"]);
    }

    /// `read-only` and `edit-worktree`, as Branchyard's presets name their
    /// tools: no command, and writes denied only by the first.
    #[test]
    fn the_presets_that_deny_the_shell_allow_no_command() {
        let shell = ["Bash", "run_command", "shell"];
        let read_only = Permissions {
            rules: shell
                .iter()
                .chain(&["write_file", "Edit"])
                .map(|t| rule(t, false))
                .chain([rule("Read", true)])
                .collect(),
            by: Some("/usr/local/bin/by".into()),
            ..Permissions::new(Fallback::Deny)
        };
        let written = rules(&context(&[]), &read_only);
        assert!(written.allow.is_empty(), "{written:?}");
        assert_eq!(written.deny, ["write_file(*)"]);
        let edit_worktree = Permissions {
            rules: shell
                .iter()
                .map(|t| rule(t, false))
                .chain([rule("write_file", true)])
                .collect(),
            ..read_only
        };
        assert!(rules(&context(&[]), &edit_worktree).is_empty());
    }
}
