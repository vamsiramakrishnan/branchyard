// Derived from Scion (https://github.com/GoogleCloudPlatform/scion) at
// d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338: harnesses/hermes/provision.py
// and the auth and Vertex AI tests of harnesses/hermes/provision_test.py.
// Copyright 2026 Google LLC. Licensed under the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to a sans-IO planner.
// `~/.hermes/.env` is merged line by line instead of replaced; Vertex AI's
// project and region come only from the secrets given; `HERMES_HOME` is the
// harness's own home rather than /home/scion. Not ported:
// `HERMES_YOLO_MODE`, `HERMES_QUIET` and `HERMES_ACCEPT_HOOKS`, because
// Branchyard's ACP driver routes every permission request and accepts no
// hooks on the harness's behalf; the `mcp.json` writer and AGENTS.md
// projection, because the driver passes both in the session; the Hermes
// dashboard sidecar, which only Scion's init process starts.

//! Hermes: `hermes-acp`.

use crate::auth::{Method, Spec};
use crate::edit::Edit;
use crate::{
    needs_private_home, pass_session, unsupported, unused, Context, EnvVar, Plan, Provisioner,
    Refused, Via,
};

const DOTENV: &str = ".hermes/.env";

/// Scion's model when Vertex AI is used and none is asked for.
pub const VERTEX_DEFAULT_MODEL: &str = "google/gemini-2.5-flash";

pub const AUTH: Spec = Spec {
    harness: "hermes",
    methods: &[
        Method::env(
            "api-key",
            &["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GOOGLE_API_KEY"],
            "set ANTHROPIC_API_KEY, OPENAI_API_KEY, or GOOGLE_API_KEY",
        ),
        Method::env(
            "vertex-ai",
            &["VERTEX_PROJECT_ID", "GOOGLE_CLOUD_PROJECT"],
            "set VERTEX_PROJECT_ID or GOOGLE_CLOUD_PROJECT (with ADC available to the harness) \
             for Vertex AI",
        ),
    ],
};

/// Region secrets, in Scion's order.
const REGIONS: &[&str] = &[
    "VERTEX_REGION",
    "GOOGLE_CLOUD_REGION",
    "GOOGLE_CLOUD_LOCATION",
];

/// The Vertex AI region: the first region secret given, else us-central1.
pub fn vertex_region(context: &Context) -> String {
    REGIONS
        .iter()
        .find_map(|name| context.secret(name).filter(|v| !v.is_empty()))
        .unwrap_or("us-central1")
        .to_owned()
}

pub(crate) struct Hermes;

impl Provisioner for Hermes {
    fn harness(&self) -> &'static str {
        "hermes"
    }

    fn scion(&self) -> Option<&'static str> {
        Some("hermes")
    }

    fn plan(&self, context: &Context) -> Result<Plan, Refused> {
        let why = "Scion's Hermes provisioner has no setting for it";
        if context.effort.is_some() {
            return Err(unsupported(context, "reasoning effort", why));
        }
        if context.telemetry.is_some() {
            return Err(unsupported(context, "telemetry", why));
        }
        let mut plan = Plan::default();
        pass_session(context, &mut plan);
        let given: Vec<&str> = context.secrets.iter().map(|s| s.name.as_str()).collect();
        let resolved = AUTH.select(&given, context.auth.as_deref())?;
        let mut model = context.model.clone();
        if let Some(resolved) = resolved {
            needs_private_home(context, "Hermes's .env")?;
            plan.auth = Some(resolved.method.to_owned());
            let key = resolved.env_key.expect("an env method has a key");
            // Hermes reads `~/.hermes/.env` into its own environment
            // (python-dotenv); whether its terminal tool filters it could
            // not be checked offline, so it is taken to reach tool
            // commands.
            let pairs = match resolved.method {
                "api-key" => {
                    plan.deliver(
                        key,
                        Via::File {
                            path: DOTENV.into(),
                        },
                        true,
                    );
                    vec![(
                        key.to_owned(),
                        context.secret(key).unwrap_or_default().to_owned(),
                    )]
                }
                "vertex-ai" => {
                    let project = context
                        .secret("VERTEX_PROJECT_ID")
                        .or_else(|| context.secret("GOOGLE_CLOUD_PROJECT"))
                        .unwrap_or_default()
                        .to_owned();
                    let pairs = vec![
                        ("VERTEX_PROJECT_ID".to_owned(), project),
                        ("VERTEX_REGION".to_owned(), vertex_region(context)),
                    ];
                    for (name, value) in &pairs {
                        plan.set_env(EnvVar::secret(name, value.clone()));
                    }
                    let source = match context.secret("VERTEX_PROJECT_ID") {
                        Some(_) => "VERTEX_PROJECT_ID",
                        None => "GOOGLE_CLOUD_PROJECT",
                    };
                    plan.deliver(
                        source,
                        Via::Env {
                            var: "VERTEX_PROJECT_ID".into(),
                        },
                        true,
                    );
                    if let Some(region) = REGIONS.iter().find(|r| context.secret(r).is_some()) {
                        plan.deliver(
                            region,
                            Via::Env {
                                var: "VERTEX_REGION".into(),
                            },
                            true,
                        );
                    }
                    model.get_or_insert_with(|| VERTEX_DEFAULT_MODEL.to_owned());
                    pairs
                }
                _ => unreachable!("every method of AUTH is handled"),
            };
            plan.edit(DOTENV, true, Edit::Dotenv(pairs));
            plan.set_env(EnvVar::plain("HERMES_HOME", context.home_path(".hermes")));
        }
        let mut names = AUTH.names();
        names.extend(REGIONS);
        plan.unused_secrets = unused(context, &names);
        if let Some(model) = model {
            plan.set_env(EnvVar::plain("HERMES_INFERENCE_MODEL", model));
        }
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Protocol, Secret};

    fn context(secrets: &[(&str, &str)]) -> Context {
        let mut context = Context::new("hermes", Protocol::Acp, "/h", "/w");
        context.private_home = true;
        context.secrets = secrets.iter().map(|(n, v)| Secret::new(*n, *v)).collect();
        context
    }

    fn env(plan: &Plan, name: &str) -> Option<String> {
        plan.env
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.value.clone())
    }

    // From Scion's hermes/provision_test.py.

    #[test]
    fn api_key_precedence() {
        for (given, want) in [
            (
                &["GOOGLE_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY"][..],
                "ANTHROPIC_API_KEY",
            ),
            (&["GOOGLE_API_KEY", "OPENAI_API_KEY"][..], "OPENAI_API_KEY"),
            (&["GOOGLE_API_KEY"][..], "GOOGLE_API_KEY"),
        ] {
            let got = AUTH.select(given, None).unwrap().unwrap();
            assert_eq!((got.method, got.env_key), ("api-key", Some(want)));
        }
        let got = AUTH.select(&["VERTEX_PROJECT_ID"], None).unwrap().unwrap();
        assert_eq!(got.method, "vertex-ai");
        let got = AUTH
            .select(&["GOOGLE_CLOUD_PROJECT"], None)
            .unwrap()
            .unwrap();
        assert_eq!(got.method, "vertex-ai");
        let got = AUTH
            .select(&["GOOGLE_CLOUD_PROJECT", "OPENAI_API_KEY"], None)
            .unwrap()
            .unwrap();
        assert_eq!(got.method, "api-key");
        assert!(AUTH.select(&[], Some("bogus")).is_err());
    }

    #[test]
    fn vertex_ai_sets_default_model_and_env() {
        let plan = Hermes
            .plan(&context(&[("GOOGLE_CLOUD_PROJECT", "proj")]))
            .unwrap();
        assert_eq!(env(&plan, "VERTEX_PROJECT_ID").unwrap(), "proj");
        assert_eq!(env(&plan, "VERTEX_REGION").unwrap(), "us-central1");
        assert_eq!(
            env(&plan, "HERMES_INFERENCE_MODEL").unwrap(),
            VERTEX_DEFAULT_MODEL
        );
        assert!(env(&plan, "HERMES_YOLO_MODE").is_none());
    }

    #[test]
    fn vertex_ai_respects_explicit_model() {
        let mut context = context(&[("VERTEX_PROJECT_ID", "p")]);
        context.model = Some("anthropic/claude".into());
        let plan = Hermes.plan(&context).unwrap();
        assert_eq!(
            env(&plan, "HERMES_INFERENCE_MODEL").unwrap(),
            "anthropic/claude"
        );
    }

    #[test]
    fn vertex_region_fallback_order() {
        let all = [
            ("VERTEX_REGION", "a"),
            ("GOOGLE_CLOUD_REGION", "b"),
            ("GOOGLE_CLOUD_LOCATION", "c"),
        ];
        assert_eq!(vertex_region(&context(&all)), "a");
        assert_eq!(vertex_region(&context(&all[1..])), "b");
        assert_eq!(vertex_region(&context(&all[2..])), "c");
        assert_eq!(vertex_region(&context(&[])), "us-central1");
    }
}
