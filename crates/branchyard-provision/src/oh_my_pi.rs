//! Oh My Pi: `oh-my-pi-acp`. `omp acp` takes MCP servers and instructions
//! on the session channel, and asks its client before shell commands, file
//! deletions and moves (<https://omp.sh/docs/acp>), which the ACP driver
//! answers from the branch's policy. Three things have no place on that
//! channel and are written into its configuration instead, in a private
//! home: the model (ACP v1 has no model parameter), the thinking level, and
//! a policy that denies file edits or the web, since omp's reads, its
//! non-destructive writes and its web tools skip the client's gate.
//!
//! The configuration is `~/.omp/agent/config.yml`. It is YAML, and omp
//! 18.10.3 reads the JSON object Branchyard writes there (JSON is YAML;
//! checked with a headless run), so it is edited like the other harnesses'
//! settings: `modelRoles.default` selects the model (`google/gemini-3.5-flash`,
//! or a bare name omp matches), `defaultThinkingLevel` the thinking level
//! (`minimal`, `low`, `medium`, `high`, `xhigh`, `max` or `auto`), and
//! `tools.approval` holds per-tool policies (`allow`, `prompt` or `deny`),
//! which omp enforces itself: a denied `write` ends the call with `Tool
//! "write" is blocked by user policy`, and the run goes on. Each turn
//! writes what it asks for and takes back what it no longer does, so a send
//! under a wider policy, or without a model, undoes the earlier turn's
//! settings.
//!
//! Provider keys are read from its environment (`GEMINI_API_KEY`,
//! `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY`, ...), so
//! each secret named like one (`*_API_KEY`) is set there under its own
//! name, as for a harness without a provisioner. Its stored logins (`omp
//! login PROVIDER`, kept in `~/.omp/agent/agent.db`) are not provisioned:
//! `omp auth export` writes them as JSON for `omp auth import`, and neither
//! is a file Branchyard can place.

use serde_json::json;

use crate::edit::{path, Edit, JsonEdit};
use crate::generic::KEY_SUFFIX;
use crate::permissions::{Permissions, PermissionsWritten};
use crate::{
    needs_private_home, pass_session, unsupported, unused, Context, Plan, Provisioner, Refused,
};

/// omp's configuration, in its agent directory (`~/.omp/agent`; omp also
/// reads `PI_CODING_AGENT_DIR`, which Branchyard does not set).
pub const CONFIG: &str = ".omp/agent/config.yml";

/// The tools whose policy is written, by the names omp's per-tool policies
/// take, each with the ACP kind whose decision it follows when no rule
/// names the tool itself: omp's file tools skip the ACP gate on a
/// non-destructive write, so they follow the kind an ACP edit request would
/// carry; its web tools likewise.
pub const GATED_TOOLS: &[(&str, &str)] = &[
    ("write", "acp:edit"),
    ("edit", "acp:edit"),
    ("web_search", "acp:fetch"),
    ("browser", "acp:fetch"),
];

/// The tools of [`GATED_TOOLS`] that `permissions` denies: by a rule naming
/// the tool itself first, else by its ACP kind under the policy's fallback.
/// Never wider than the policy: a tool it allows, or would ask a person
/// about, keeps omp's own default, which asks the ACP client for commands,
/// deletions and moves and nothing else.
pub fn denied(permissions: &Permissions) -> Vec<String> {
    GATED_TOOLS
        .iter()
        .filter(|(tool, kind)| {
            permissions.rule(tool).or_else(|| permissions.decide(kind)) == Some(false)
        })
        .map(|(tool, _)| (*tool).to_owned())
        .collect()
}

pub(crate) struct OhMyPi;

impl Provisioner for OhMyPi {
    fn harness(&self) -> &'static str {
        "oh-my-pi"
    }

    fn scion(&self) -> Option<&'static str> {
        None
    }

    fn plan(&self, context: &Context) -> Result<Plan, Refused> {
        if context.telemetry.is_some() {
            return Err(unsupported(
                context,
                "telemetry",
                "omp 18.10.3 has no telemetry setting Branchyard knows",
            ));
        }
        if context.auth.is_some() {
            return Err(unsupported(
                context,
                "an auth method",
                "omp reads provider keys from its environment, and its logins (omp login) are \
                 not files Branchyard can place",
            ));
        }
        let mut plan = Plan::default();
        let keys: Vec<&str> = context
            .secrets
            .iter()
            .map(|s| s.name.as_str())
            .filter(|name| name.ends_with(KEY_SUFFIX))
            .collect();
        if !keys.is_empty() {
            plan.auth = Some("api-key".into());
        }
        for key in &keys {
            // omp's shell tool inherits its environment: taken to reach
            // tool commands.
            plan.secret_env(key, key, context.secret(key).unwrap_or_default(), true);
        }
        plan.unused_secrets = unused(context, &keys);
        pass_session(context, &mut plan);

        // config.yml: the model and the thinking level, set when asked for
        // and taken back when not, and a `deny` for each tool the policy
        // denies that omp would otherwise run without asking.
        if !context.private_home {
            if context.model.is_some() {
                needs_private_home(
                    context,
                    "The model (modelRoles.default in omp's config.yml)",
                )?;
            }
            if context.effort.is_some() {
                needs_private_home(
                    context,
                    "Reasoning effort (defaultThinkingLevel in omp's config.yml)",
                )?;
            }
            // A home that is yours keeps your policies; the ACP gate still
            // asks Branchyard about commands, deletions and moves.
            return Ok(plan);
        }
        let mut edits = Vec::new();
        for (key, value) in [
            (
                path(&["modelRoles", "default"]),
                context.model.as_deref().map(|model| json!(model)),
            ),
            (
                path(&["defaultThinkingLevel"]),
                context.effort.map(|effort| json!(effort.as_str())),
            ),
        ] {
            edits.push(match value {
                Some(value) => JsonEdit::Set(key, value),
                None => JsonEdit::Remove(key),
            });
        }
        let deny = context.permissions.as_ref().map(denied).unwrap_or_default();
        for (tool, _) in GATED_TOOLS {
            let key = path(&["tools", "approval", tool]);
            edits.push(match deny.iter().any(|t| t == tool) {
                true => JsonEdit::Set(key, json!("deny")),
                // Only a `deny` is Branchyard's to take back.
                false => JsonEdit::RemoveIf(key, json!("deny")),
            });
        }
        plan.edit(
            CONFIG,
            false,
            Edit::Json {
                edits,
                comment_lines: false,
            },
        );
        if context.permissions.is_some() {
            plan.permissions = Some(PermissionsWritten {
                path: CONFIG.into(),
                allow: Vec::new(),
                deny,
            });
        }
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{Fallback, PermissionRule};
    use crate::{Effort, Protocol, Secret, Via};

    fn context() -> Context {
        let mut context = Context::new("oh-my-pi", Protocol::Acp, "/h", "/w");
        context.private_home = true;
        context
    }

    /// The JSON edits the plan makes to omp's config.yml.
    fn config_edits(plan: &Plan) -> Vec<JsonEdit> {
        let config = plan.files.iter().find(|f| f.path == CONFIG).unwrap();
        config
            .edits
            .iter()
            .flat_map(|edit| match edit {
                Edit::Json { edits, .. } => edits.clone(),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    fn rules(tools: &[&str], allow: bool, fallback: Fallback) -> Permissions {
        Permissions {
            rules: tools
                .iter()
                .map(|tool| PermissionRule {
                    tool: (*tool).into(),
                    allow,
                    delegation_by: None,
                })
                .collect(),
            ..Permissions::new(fallback)
        }
    }

    #[test]
    fn provider_keys_reach_omp_and_its_tools_and_the_model_goes_in_its_config() {
        let mut context = context();
        context.secrets = vec![
            Secret::new("GEMINI_API_KEY", "k1"),
            Secret::new("GH_TOKEN", "t"),
        ];
        context.model = Some("google/gemini-3.5-flash".into());
        context.effort = Some(Effort::Low);
        let plan = OhMyPi.plan(&context).unwrap();
        assert_eq!(plan.auth.as_deref(), Some("api-key"));
        assert!(plan.secrets.iter().any(|d| d.secret == "GEMINI_API_KEY"
            && d.via
                == Via::Env {
                    var: "GEMINI_API_KEY".into()
                }
            && d.tool_env));
        assert_eq!(plan.unused_secrets, ["GH_TOKEN"]);
        // The model is consumed here, not passed to the ACP driver, which
        // has no parameter for it.
        assert_eq!(plan.session.model, None);
        let edits = config_edits(&plan);
        assert!(edits.contains(&JsonEdit::Set(
            path(&["modelRoles", "default"]),
            json!("google/gemini-3.5-flash")
        )));
        assert!(edits.contains(&JsonEdit::Set(
            path(&["defaultThinkingLevel"]),
            json!("low")
        )));
        // No policy: nothing denied, and an earlier deny is taken back.
        assert!(edits.contains(&JsonEdit::RemoveIf(
            path(&["tools", "approval", "write"]),
            json!("deny")
        )));
        assert_eq!(plan.permissions, None);
    }

    #[test]
    fn a_policy_that_denies_edits_or_the_web_denies_omps_ungated_tools() {
        // `read-only`: edits, commands and the web denied by kind.
        let read_only = rules(
            &["acp:edit", "acp:execute", "acp:fetch"],
            false,
            Fallback::Deny,
        );
        assert_eq!(
            denied(&read_only),
            ["write", "edit", "web_search", "browser"]
        );
        // `edit-worktree`: edits allowed, the web denied by the fallback.
        let edit_worktree = rules(&["acp:edit"], true, Fallback::Deny);
        assert_eq!(denied(&edit_worktree), ["web_search", "browser"]);
        // `full`: nothing denied.
        assert!(denied(&Permissions::new(Fallback::Allow)).is_empty());
        // What would ask a person is left to omp, which asks nobody.
        assert!(denied(&Permissions::new(Fallback::Ask)).is_empty());
        // A rule naming omp's own tool wins over its kind.
        let mut named = rules(&["acp:edit"], true, Fallback::Allow);
        named.rules.push(PermissionRule {
            tool: "write".into(),
            allow: false,
            delegation_by: None,
        });
        assert_eq!(denied(&named), ["write"]);

        let mut context = context();
        context.permissions = Some(read_only);
        let plan = OhMyPi.plan(&context).unwrap();
        let edits = config_edits(&plan);
        for tool in ["write", "edit", "web_search", "browser"] {
            assert!(edits.contains(&JsonEdit::Set(
                path(&["tools", "approval", tool]),
                json!("deny")
            )));
        }
        assert_eq!(
            plan.permissions,
            Some(PermissionsWritten {
                path: CONFIG.into(),
                allow: Vec::new(),
                deny: ["write", "edit", "web_search", "browser"]
                    .iter()
                    .map(|t| (*t).to_owned())
                    .collect(),
            })
        );
    }

    #[test]
    fn a_home_that_is_yours_gets_no_config_and_refuses_a_model() {
        let mut context = context();
        context.private_home = false;
        context.permissions = Some(rules(&["acp:edit"], false, Fallback::Deny));
        let plan = OhMyPi.plan(&context).unwrap();
        assert!(plan.files.is_empty() && plan.permissions.is_none());
        context.model = Some("gemini-3.5-flash".into());
        let refused = OhMyPi.plan(&context).unwrap_err();
        assert!(refused.0.contains("modelRoles.default"), "{}", refused.0);
        context.model = None;
        context.effort = Some(Effort::High);
        let refused = OhMyPi.plan(&context).unwrap_err();
        assert!(refused.0.contains("defaultThinkingLevel"), "{}", refused.0);
    }

    #[test]
    fn telemetry_and_an_explicit_auth_method_are_refused() {
        let mut context = context();
        context.auth = Some("api-key".into());
        assert!(OhMyPi.plan(&context).unwrap_err().0.contains("omp login"));
        let mut context = self::context();
        context.telemetry = Some(crate::Telemetry::parse("http://127.0.0.1:4317").unwrap());
        assert!(OhMyPi.plan(&context).unwrap_err().0.contains("telemetry"));
    }
}
