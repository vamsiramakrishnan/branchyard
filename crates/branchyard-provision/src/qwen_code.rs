//! Qwen Code: `qwen-code-acp`. `qwen --acp` takes MCP servers and
//! instructions on the session channel, and its login from its
//! environment: Qwen Code 0.26.0 logs in to an OpenAI-compatible endpoint
//! from the environment only when `OPENAI_API_KEY`, `OPENAI_BASE_URL` and
//! a model (`OPENAI_MODEL`) are all set (`getAuthTypeFromEnv` in its
//! bundle), which is how it ran on Z.ai's GLM Coding Plan endpoint in round
//! 8 (9 of 9, 25 October 2026). So the three are set in its environment
//! from the secrets of those names, and the model a run asks for goes into
//! `OPENAI_MODEL` (ACP v1 has no model parameter), with or without a
//! private home. Any other provider key (`*_API_KEY`) is set under its own
//! name, as for a harness without a provisioner. Qwen's shell tool inherits
//! its environment, so each is taken to reach tool commands.

use crate::generic::KEY_SUFFIX;
use crate::{pass_session, unsupported, unused, Context, EnvVar, Plan, Provisioner, Refused};

/// The endpoint variable, given as a secret beside the key.
pub const BASE_URL: &str = "OPENAI_BASE_URL";
/// The model variable.
pub const MODEL: &str = "OPENAI_MODEL";

pub(crate) struct QwenCode;

impl Provisioner for QwenCode {
    fn harness(&self) -> &'static str {
        "qwen-code"
    }

    fn scion(&self) -> Option<&'static str> {
        None
    }

    fn plan(&self, context: &Context) -> Result<Plan, Refused> {
        let why = "Qwen Code reads only its login and model from its environment here";
        if context.effort.is_some() {
            return Err(unsupported(context, "reasoning effort", why));
        }
        if context.telemetry.is_some() {
            return Err(unsupported(context, "telemetry", why));
        }
        if context.auth.is_some() {
            return Err(unsupported(context, "an auth method", why));
        }
        let mut plan = Plan::default();
        let mut used: Vec<&str> = context
            .secrets
            .iter()
            .map(|s| s.name.as_str())
            .filter(|name| name.ends_with(KEY_SUFFIX) || [BASE_URL, MODEL].contains(name))
            .collect();
        if used.iter().any(|name| name.ends_with(KEY_SUFFIX)) {
            plan.auth = Some("api-key".into());
        }
        for name in &used {
            plan.secret_env(name, name, context.secret(name).unwrap_or_default(), true);
        }
        if let Some(model) = &context.model {
            // The run's model wins over one given as a secret.
            plan.secrets.retain(|d| d.secret != MODEL);
            used.retain(|name| *name != MODEL);
            plan.set_env(EnvVar::plain(MODEL, model.clone()));
        }
        plan.unused_secrets = unused(context, &used);
        pass_session(context, &mut plan);
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Protocol, Secret, Via};

    fn context() -> Context {
        let mut context = Context::new("qwen-code", Protocol::Acp, "/h", "/w");
        context.private_home = true;
        context
    }

    #[test]
    fn the_openai_login_and_other_keys_reach_its_environment() {
        let mut context = context();
        context.secrets = vec![
            Secret::new("OPENAI_API_KEY", "k"),
            Secret::new("OPENAI_BASE_URL", "https://example.test/v4"),
            Secret::new("GEMINI_API_KEY", "g"),
            Secret::new("GH_TOKEN", "t"),
        ];
        let plan = QwenCode.plan(&context).unwrap();
        assert_eq!(plan.auth.as_deref(), Some("api-key"));
        let env: Vec<&str> = plan.env.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(env, ["OPENAI_API_KEY", "OPENAI_BASE_URL", "GEMINI_API_KEY"]);
        assert!(plan
            .secrets
            .iter()
            .all(|d| d.tool_env && matches!(&d.via, Via::Env { var } if *var == d.secret)));
        assert_eq!(plan.unused_secrets, ["GH_TOKEN"]);
        assert!(plan.files.is_empty() && plan.session.model.is_none());
    }

    #[test]
    fn a_model_goes_into_openai_model_without_a_private_home_too() {
        let mut context = Context::new("qwen-code", Protocol::Acp, "/h", "/w");
        context.model = Some("glm-4.5-air".into());
        let plan = QwenCode.plan(&context).unwrap();
        assert_eq!(
            plan.env,
            [EnvVar::plain("OPENAI_MODEL", "glm-4.5-air")],
            "{:?}",
            plan.env
        );
        assert!(plan.session.model.is_none());
        // Over a model given as a secret, which is then unused.
        let mut context = self::context();
        context.secrets = vec![Secret::new("OPENAI_MODEL", "glm-4.6")];
        context.model = Some("glm-5.3".into());
        let plan = QwenCode.plan(&context).unwrap();
        assert_eq!(plan.env, [EnvVar::plain("OPENAI_MODEL", "glm-5.3")]);
        assert!(plan.secrets.is_empty());
        assert_eq!(plan.unused_secrets, ["OPENAI_MODEL"]);
    }

    #[test]
    fn effort_telemetry_and_auth_methods_are_refused() {
        let mut context = context();
        context.effort = Some(crate::Effort::High);
        assert!(QwenCode.plan(&context).is_err());
        let mut context = self::context();
        context.auth = Some("api-key".into());
        assert!(QwenCode.plan(&context).is_err());
    }
}
