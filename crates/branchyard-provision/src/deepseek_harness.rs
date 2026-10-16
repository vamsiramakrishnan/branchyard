//! DeepSeek Harness: `deepseek-harness-acp` (`dsh --profile acp`, dsh
//! 0.2.0-rc.2). Over ACP it takes MCP servers and instructions on the
//! session channel and asks its client before commands, which the ACP
//! driver answers from the branch's policy. It has no model parameter: the
//! model, and the provider that serves it, come from its configuration,
//! composed from `cordis.patch.yml` under `$DSH_HOME` (default `~/.dsh`), a
//! YAML list of plugin patches (`dsh --profile acp --dump-config` shows the
//! composed result). Provider keys are read from its environment, by the
//! variable each provider's `apiKeyEnv` names (`DEEPSEEK_API_KEY` for its
//! own `deepseek-official`).
//!
//! What this provisioner does, in a private home:
//!
//! - **Keys**, as the generic provisioner: each secret named like one
//!   (`*_API_KEY`) in the harness's environment and its tools', under its
//!   own name; any other secret is reported unused.
//! - **The model**, when the turn has one (`--model`, a fleet candidate's):
//!   [`PATCH`] sets the `agent-default-model` and `acp` plugins to it. On
//!   the model gateway ([`Context::model_gateway`]) the file also adds a
//!   provider `branchyard` to the `llm-pi-ai` plugin: OpenAI chat
//!   completions (`api: openai-completions`) at the gateway's `/openai/v1`,
//!   keyed by `OPENAI_API_KEY`, which the engine sets to the turn's token,
//!   with the model under it; when the gateway serves only Anthropic's API,
//!   Anthropic Messages (`anthropic-messages`) at `/anthropic` with
//!   `ANTHROPIC_API_KEY` instead. Off the gateway, a model written
//!   `PROVIDER/MODEL` names one of the harness's providers (its own
//!   `deepseek-official`, or one another patch file in the home configures);
//!   a bare `gemini-*` model with a `GEMINI_API_KEY` (or `GOOGLE_API_KEY`)
//!   secret gets a `gemini` provider at Gemini's OpenAI-compatible endpoint
//!   (`api: openai-completions`, `compat: {supportsStore: false}`, since the
//!   endpoint rejects the OpenAI-only `store` field), which is how the
//!   harness ran on `gemini-2.5-flash-lite` in qualification; any other bare
//!   model is `deepseek-official`'s. The gateway without a model is refused:
//!   the provider needs the model's id.
//!
//! The patch file is Branchyard's: written whole, and left as it is by a
//! turn that names no model. Reasoning effort and telemetry are refused:
//! the provider profile has a `reasoning` level, but its values were not
//! checked against the harness.

use crate::edit::Edit;
use crate::generic::KEY_SUFFIX;
use crate::{
    needs_private_home, pass_session, unsupported, unused, Context, Plan, Provisioner, Refused,
};

/// The patch file this provisioner writes, under the default `DSH_HOME`.
pub const PATCH: &str = ".dsh/cordis.patch.yml";

/// The provider written for the model gateway.
pub const GATEWAY_PROVIDER: &str = "branchyard";

/// The harness's own provider, which a bare model names.
pub const OWN_PROVIDER: &str = "deepseek-official";

/// The provider written for a bare `gemini-*` model off the gateway, at
/// Gemini's OpenAI-compatible endpoint.
pub const GEMINI_PROVIDER: &str = "gemini";
const GEMINI_OPENAI_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";
const GEMINI_KEYS: [&str; 2] = ["GEMINI_API_KEY", "GOOGLE_API_KEY"];

pub(crate) struct DeepSeekHarness;

impl Provisioner for DeepSeekHarness {
    fn harness(&self) -> &'static str {
        "deepseek-harness"
    }

    fn scion(&self) -> Option<&'static str> {
        None
    }

    fn plan(&self, context: &Context) -> Result<Plan, Refused> {
        if context.effort.is_some() {
            return Err(unsupported(
                context,
                "reasoning effort",
                "dsh 0.2.0-rc.2's provider profile has a reasoning level whose values Branchyard \
                 has not checked",
            ));
        }
        if context.telemetry.is_some() {
            return Err(unsupported(
                context,
                "telemetry",
                "dsh 0.2.0-rc.2 has no telemetry setting Branchyard knows",
            ));
        }
        if context.auth.is_some() {
            return Err(unsupported(
                context,
                "an auth method",
                "dsh reads provider keys from its environment, by the variable each provider's \
                 apiKeyEnv names",
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
            // Its shell tool inherits the environment: taken to reach tool
            // commands.
            plan.secret_env(key, key, context.secret(key).unwrap_or_default(), true);
        }
        plan.unused_secrets = unused(context, &keys);
        pass_session(context, &mut plan);

        let Some(model) = context.model.as_deref() else {
            if context.model_gateway.is_some() {
                return Err(unsupported(
                    context,
                    "the model gateway without a model",
                    "dsh takes its model from a provider in its cordis.patch.yml, which \
                     Branchyard writes from --model; name the model the gateway serves",
                ));
            }
            return Ok(plan);
        };
        needs_private_home(
            context,
            "The model (agent-default-model and acp in dsh's cordis.patch.yml)",
        )?;
        let text = match &context.model_gateway {
            Some(gateway) => {
                let (api, key, path) = match (gateway.openai, gateway.anthropic) {
                    (true, _) => ("openai-completions", "OPENAI_API_KEY", "/openai/v1"),
                    (false, true) => ("anthropic-messages", "ANTHROPIC_API_KEY", "/anthropic"),
                    (false, false) => {
                        return Err(unsupported(
                            context,
                            "the model gateway",
                            "it serves neither OpenAI's API nor Anthropic's",
                        ))
                    }
                };
                let url = format!("{}{path}", gateway.url.trim_end_matches('/'));
                patch(Some((api, key, &url, false)), GATEWAY_PROVIDER, model)
            }
            None => match model.split_once('/') {
                Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
                    patch(None, provider, model)
                }
                _ => match GEMINI_KEYS.iter().find(|k| keys.contains(k)) {
                    Some(key) if model.starts_with("gemini-") => patch(
                        Some(("openai-completions", key, GEMINI_OPENAI_URL, true)),
                        GEMINI_PROVIDER,
                        model,
                    ),
                    _ => patch(None, OWN_PROVIDER, model),
                },
            },
        };
        plan.edit(PATCH, false, Edit::Put(text));
        Ok(plan)
    }
}

/// The patch file: a provider when `endpoint` says how (`api`, key
/// variable, base URL, whether the endpoint rejects OpenAI's `store`), and
/// the default model for the agent and for ACP sessions.
fn patch(endpoint: Option<(&str, &str, &str, bool)>, provider: &str, model: &str) -> String {
    let mut text = String::from(
        "# Written by Branchyard for this branch's turn (docs/provisioning.md, \"DeepSeek \
         Harness\"); edits are replaced.\n",
    );
    if let Some((api, key, url, no_store)) = endpoint {
        text.push_str(&format!(
            "- id: llm-pi-ai\n  config:\n    providers:\n      {}:\n        apiKeyEnv: {key}\n        \
             api: {api}\n        baseURL: {}\n",
            yaml(provider),
            yaml(url),
        ));
        if no_store {
            text.push_str("        compat:\n          supportsStore: false\n");
        }
        text.push_str(&format!(
            "        models:\n          - id: {}\n",
            yaml(model)
        ));
    }
    for plugin in ["agent-default-model", "acp"] {
        text.push_str(&format!(
            "- id: {plugin}\n  config:\n    provider: {}\n    model: {}\n",
            yaml(provider),
            yaml(model)
        ));
    }
    text
}

/// A double-quoted YAML scalar.
fn yaml(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ModelGateway;
    use crate::{Effort, Protocol, Secret, Via};

    fn context() -> Context {
        let mut context = Context::new("deepseek-harness", Protocol::Acp, "/h", "/w");
        context.private_home = true;
        context
    }

    fn patch_text(plan: &Plan) -> String {
        let file = plan.files.iter().find(|f| f.path == PATCH).unwrap();
        match &file.edits[..] {
            [Edit::Put(text)] => text.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn keys_reach_the_harness_and_the_gateway_becomes_a_provider() {
        let mut context = context();
        context.secrets = vec![
            Secret::new("GEMINI_API_KEY", "k"),
            Secret::new("GH_TOKEN", "t"),
        ];
        context.model = Some("gemini-2.5-flash-lite".into());
        context.model_gateway = Some(ModelGateway {
            url: "http://127.0.0.1:41100/".into(),
            anthropic: true,
            openai: true,
        });
        context.mcp_servers = vec![crate::McpServer {
            name: "docs".into(),
            command: "/bin/docs".into(),
            args: Vec::new(),
            env: Vec::new(),
        }];
        let plan = DeepSeekHarness.plan(&context).unwrap();
        assert_eq!(plan.auth.as_deref(), Some("api-key"));
        assert!(plan.secrets.iter().any(|d| d.secret == "GEMINI_API_KEY"
            && d.via
                == Via::Env {
                    var: "GEMINI_API_KEY".into()
                }
            && d.tool_env));
        assert_eq!(plan.unused_secrets, ["GH_TOKEN"]);
        // MCP servers travel on the session; the model is consumed here.
        assert_eq!(plan.session.mcp_servers.len(), 1);
        assert_eq!(plan.session.model, None);
        assert_eq!(
            patch_text(&plan),
            "# Written by Branchyard for this branch's turn (docs/provisioning.md, \"DeepSeek \
             Harness\"); edits are replaced.\n\
             - id: llm-pi-ai\n  config:\n    providers:\n      \"branchyard\":\n        \
             apiKeyEnv: OPENAI_API_KEY\n        api: openai-completions\n        \
             baseURL: \"http://127.0.0.1:41100/openai/v1\"\n        models:\n          \
             - id: \"gemini-2.5-flash-lite\"\n\
             - id: agent-default-model\n  config:\n    provider: \"branchyard\"\n    \
             model: \"gemini-2.5-flash-lite\"\n\
             - id: acp\n  config:\n    provider: \"branchyard\"\n    model: \"gemini-2.5-flash-lite\"\n"
        );
        // A gateway serving Anthropic alone: Messages at /anthropic.
        context.model_gateway = Some(ModelGateway {
            url: "http://127.0.0.1:41100".into(),
            anthropic: true,
            openai: false,
        });
        let text = patch_text(&DeepSeekHarness.plan(&context).unwrap());
        assert!(text.contains("apiKeyEnv: ANTHROPIC_API_KEY\n        api: anthropic-messages\n"));
        assert!(text.contains("baseURL: \"http://127.0.0.1:41100/anthropic\"\n"));
    }

    #[test]
    fn off_the_gateway_a_model_names_a_provider_or_the_harnesss_own() {
        let mut context = context();
        context.model = Some("deepseek-v4-pro".into());
        let text = patch_text(&DeepSeekHarness.plan(&context).unwrap());
        assert!(!text.contains("llm-pi-ai"), "{text}");
        assert!(text.contains("provider: \"deepseek-official\"\n    model: \"deepseek-v4-pro\"\n"));
        context.model = Some("gemini/gemini-2.5-flash-lite".into());
        let text = patch_text(&DeepSeekHarness.plan(&context).unwrap());
        assert!(!text.contains("llm-pi-ai"), "{text}");
        assert!(text.contains("provider: \"gemini\"\n    model: \"gemini-2.5-flash-lite\"\n"));
        // A bare Gemini model with a Gemini key: a provider at Gemini's
        // OpenAI-compatible endpoint, which rejects `store`.
        context.model = Some("gemini-2.5-flash-lite".into());
        context.secrets = vec![Secret::new("GEMINI_API_KEY", "k")];
        let text = patch_text(&DeepSeekHarness.plan(&context).unwrap());
        assert!(text.contains(
            "      \"gemini\":\n        apiKeyEnv: GEMINI_API_KEY\n        api: openai-completions\n        \
             baseURL: \"https://generativelanguage.googleapis.com/v1beta/openai\"\n        compat:\n          \
             supportsStore: false\n        models:\n          - id: \"gemini-2.5-flash-lite\"\n"
        ), "{text}");
        assert!(text.contains("- id: acp\n  config:\n    provider: \"gemini\"\n"));
        // Without the key, the harness's own provider.
        context.secrets = Vec::new();
        let text = patch_text(&DeepSeekHarness.plan(&context).unwrap());
        assert!(text
            .contains("provider: \"deepseek-official\"\n    model: \"gemini-2.5-flash-lite\"\n"));
        // No model: nothing is written, and an earlier file is left.
        context.model = None;
        assert!(DeepSeekHarness.plan(&context).unwrap().files.is_empty());
    }

    #[test]
    fn what_it_cannot_place_is_refused() {
        let mut context = context();
        context.model_gateway = Some(ModelGateway {
            url: "http://127.0.0.1:1".into(),
            anthropic: false,
            openai: true,
        });
        let refused = DeepSeekHarness.plan(&context).unwrap_err();
        assert!(refused.0.contains("without a model"), "{}", refused.0);
        context.model = Some("x".into());
        context.private_home = false;
        let refused = DeepSeekHarness.plan(&context).unwrap_err();
        assert!(refused.0.contains("cordis.patch.yml"), "{}", refused.0);
        let mut context = self::context();
        context.effort = Some(Effort::High);
        assert!(DeepSeekHarness
            .plan(&context)
            .unwrap_err()
            .0
            .contains("reasoning"));
        let mut context = self::context();
        context.telemetry = Some(crate::Telemetry::parse("http://127.0.0.1:4317").unwrap());
        assert!(DeepSeekHarness
            .plan(&context)
            .unwrap_err()
            .0
            .contains("telemetry"));
    }

    #[test]
    fn yaml_scalars_are_quoted_and_escaped() {
        assert_eq!(yaml("plain"), "\"plain\"");
        assert_eq!(yaml("a \"b\" \\ c"), "\"a \\\"b\\\" \\\\ c\"");
    }
}
