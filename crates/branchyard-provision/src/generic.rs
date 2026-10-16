//! Harnesses without a translated provisioner: session items pass through
//! to the driver, which accepts or refuses them, and nothing is written in
//! the home. Such a harness reads its provider keys from its environment
//! and nothing else would hand one over, so a secret the task names that is
//! one (`--secret NAME`, `NAME` ending in `_API_KEY`) is set in the
//! harness's environment under its own name, and in that of its tool
//! commands (Pi reads `ANTHROPIC_API_KEY`, Goose whichever provider's).
//! Any other secret is reported unused. DeepSeek Harness did the same here
//! until it got a provisioner of its own ([`crate::deepseek_harness`]),
//! which keeps this handling of keys.

use crate::{pass_session, unsupported, unused, Context, Plan, Provisioner, Refused};

/// The suffix of a secret that is a provider key the harness reads from
/// its environment.
pub const KEY_SUFFIX: &str = "_API_KEY";

pub(crate) struct Generic;

impl Provisioner for Generic {
    fn harness(&self) -> &'static str {
        "*"
    }

    fn scion(&self) -> Option<&'static str> {
        None
    }

    fn plan(&self, context: &Context) -> Result<Plan, Refused> {
        let why = "Branchyard has no provisioner for it";
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
        // `plan` has already refused secrets without a private home.
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
            // Whether the harness's shell tool filters it cannot be known for
            // every harness: taken to reach tool commands.
            plan.secret_env(key, key, context.secret(key).unwrap_or_default(), true);
        }
        plan.unused_secrets = unused(context, &keys);
        // The drivers pass these on or refuse them with their own reason.
        pass_session(context, &mut plan);
        plan.session.model = context.model.clone();
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Protocol, Secret, Via};

    fn context() -> Context {
        let mut context = Context::new("deepseek-harness", Protocol::Acp, "/h", "/w");
        context.private_home = true;
        context
    }

    #[test]
    fn provider_keys_reach_the_harness_and_its_tools_under_their_own_names() {
        let mut context = context();
        context.secrets = vec![
            Secret::new("GEMINI_API_KEY", "k1"),
            Secret::new("GH_TOKEN", "t"),
            Secret::new("DEEPSEEK_API_KEY", "k2"),
        ];
        let plan = Generic.plan(&context).unwrap();
        assert_eq!(plan.auth.as_deref(), Some("api-key"));
        let env: Vec<(&str, &str, bool)> = plan
            .env
            .iter()
            .map(|e| (e.name.as_str(), e.value.as_str(), e.secret))
            .collect();
        assert_eq!(
            env,
            [
                ("GEMINI_API_KEY", "k1", true),
                ("DEEPSEEK_API_KEY", "k2", true)
            ]
        );
        for key in ["GEMINI_API_KEY", "DEEPSEEK_API_KEY"] {
            assert!(plan
                .secrets
                .iter()
                .any(|d| d.secret == key && d.via == Via::Env { var: key.into() } && d.tool_env));
        }
        assert_eq!(plan.unused_secrets, ["GH_TOKEN"]);
        assert!(plan.files.is_empty());
    }

    #[test]
    fn without_a_provider_key_nothing_is_set_and_every_secret_is_unused() {
        let mut context = context();
        context.secrets = vec![Secret::new("GH_TOKEN", "t")];
        let plan = Generic.plan(&context).unwrap();
        assert_eq!(plan.auth, None);
        assert!(plan.env.is_empty() && plan.secrets.is_empty());
        assert_eq!(plan.unused_secrets, ["GH_TOKEN"]);
        let plan = Generic
            .plan(&Context::new("pi", Protocol::Acp, "/h", "/w"))
            .unwrap();
        assert!(!plan.touches_home_or_env() && plan.unused_secrets.is_empty());
    }
}
